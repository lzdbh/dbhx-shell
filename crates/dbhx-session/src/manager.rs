//! 会话注册表：`SessionId` ⇒ 一个运行中的终端，本地与远端走同一个入口。
//!
//! 前端看到的只有「一个 tab」，它不该关心背后是 ConPTY 还是 sshd 的通道，
//! 所以这一层把两种来源压成同一个 [`SessionEvent`] 流、同一组动词
//! （`write` / `resize` / `close`）。差别只留在各自的处理里：
//!
//! * 远端：`ShellSession` 自带读任务，这里的转发任务从它的通道取事件；
//!   **退出码不等于结束**，流结束才发 `Ended`（见 [`crate::shell`]）。
//! * 本地：`PtyEvent::Exited` 之后仍可能有 `Data`（读端与退出探测是两个任务），
//!   所以同样是「记住退出信息，等通道自己关闭再收尾」。
//!
//! 回收路径只有一条：转发任务看到流结束 ⇒ 摘掉条目 ⇒ 会话对象被 drop
//! ⇒ 远端的 lease 归还 ⇒ 连接进入 idle park。注册表自己从不主动断连。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use dbhx_model::{LiveEndpoint, Protocol, PtySize, SessionId, SessionInfo, SessionStatus, TerminalEncoding};
use dbhx_pty::{LocalHandle, LocalSession, LocalSpec, PtyEvent};
use dbhx_telnet::{TelnetEvent, TelnetHandle};
use tokio::sync::mpsc;

use crate::error::SessionError;
use crate::key::TransportKey;
use crate::pool::{LeaseKind, TransportPool};
use crate::shell::{ExitInfo, ShellEvent, ShellSession};

/// 事件汇聚队列长度。转发任务只做「取出来、交给 IPC」，正常消费不会积压；
/// 队列满意味着 webview 卡住了，此时背压一路传到 sshd，而不是把内存吃掉。
const HUB_QUEUE: usize = 1024;

/// 一个 tab 的可见信息。
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: SessionId,
    pub label: String,
    /// 会话种类。有 `live_endpoint` 的不一定是 SSH——telnet 也带主机+端口，
    /// 靠 `live_endpoint.is_some()` 判协议会把 telnet 误报成 SSH，所以这里显式存一份。
    pub protocol: Protocol,
    /// `None` = 本地终端
    pub live_endpoint: Option<LiveEndpoint>,
    pub pty: PtySize,
    pub encoding: TerminalEncoding,
    /// shell 通过 OSC 7 报出的目录。本地终端没有这个概念，恒为 `None`。
    pub cwd: Option<String>,
    /// 本地终端为 `None`。文件面板要靠它向池要**同一条** transport——
    /// 用 vault 记录重算一遍键会拿到「编辑过的旧 endpoint」，正好违反判等规则。
    pub transport: Option<TransportKey>,
}

impl SessionRecord {
    /// 转成跨 IPC 的形状。
    ///
    /// `status` 只会是 `Connected`：注册表里存的本来就是「活着的会话」，
    /// 「正在连」发生在 `open_remote` 的 await 里，这一层看不见中间态。
    /// 失败会以 `Err` 回到命令层，由前端把 tab 标成错误，而不是留一条 `Error` 记录。
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id,
            protocol: self.protocol,
            status: SessionStatus::Connected,
            label: self.label.clone(),
            live_endpoint: self.live_endpoint.clone(),
            pty: self.pty.clone(),
            encoding: self.encoding,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.live_endpoint.is_some()
    }
}

/// 注册表对外吐的事件，即 [`crate::shell::ShellEvent`] 加上「这是哪个 tab 的」。
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Data { id: SessionId, bytes: Arc<[u8]> },
    Cwd { id: SessionId, path: String },
    Ended { id: SessionId, info: ExitInfo },
}

/// id ⇒ 运行中的会话。
///
/// 内部是 `Arc<Inner>`：转发任务要能在会话消失后仍然安全地触达注册表，
/// 所以它持 `Weak<Inner>`——注册表先没了就什么都不做，绝不把进程钉在这里。
pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Arc<TransportPool>,
    next_id: AtomicU32,
    entries: Mutex<HashMap<SessionId, Entry>>,
    hub_tx: mpsc::Sender<SessionEvent>,
    /// 单消费者。`take` 之后就没有第二个读端了，事件既不丢也不重。
    hub_rx: Mutex<Option<mpsc::Receiver<SessionEvent>>>,
}

/// 句柄按会话种类分叉，但只分叉到「怎么控制」这一层；事件流在构造时就接好了。
enum Handle {
    Remote(Arc<ShellSession>),
    Local(LocalHandle),
    Telnet(TelnetHandle),
}

struct Entry {
    handle: Handle,
    record: SessionRecord,
}

impl SessionManager {
    pub fn new(pool: Arc<TransportPool>) -> Self {
        let (hub_tx, hub_rx) = mpsc::channel(HUB_QUEUE);
        Self {
            inner: Arc::new(Inner {
                pool,
                next_id: AtomicU32::new(1),
                entries: Mutex::new(HashMap::new()),
                hub_tx,
                hub_rx: Mutex::new(Some(hub_rx)),
            }),
        }
    }

    /// 取走事件接收端。只能成功一次——终端字节流有两个读者就会随机丢一半。
    pub fn take_events(&self) -> Option<mpsc::Receiver<SessionEvent>> {
        self.inner.hub_rx.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// 本地终端。必须在 tokio 运行时上下文中调用（`LocalSession::spawn` 内部起任务）。
    pub fn open_local(&self, spec: &LocalSpec, label: String, encoding: TerminalEncoding) -> Result<SessionRecord, SessionError> {
        let session = LocalSession::spawn(spec)?;
        let id = self.inner.alloc_id();
        let (handle, mut events) = session.split();
        let record = SessionRecord {
            id,
            label,
            protocol: Protocol::Local,
            live_endpoint: None,
            pty: spec.pty.clone(),
            encoding,
            cwd: None,
            transport: None,
        };
        self.inner.insert(record.clone(), Handle::Local(handle));

        let weak = Arc::downgrade(&self.inner);
        let tx = self.inner.hub_tx.clone();
        tokio::spawn(async move {
            // 退出信息先攒着：Exited 之后 reader 还可能再送几行才 EOF
            let mut exit = ExitInfo::default();
            loop {
                match events.recv().await {
                    Some(PtyEvent::Data(bytes)) => {
                        if tx.send(SessionEvent::Data { id, bytes: Arc::from(bytes) }).await.is_err() {
                            break;
                        }
                    }
                    Some(PtyEvent::Exited(info)) => exit = ExitInfo { exit_status: info.code, signal: info.signal },
                    // 两个发送端都 drop 了：子进程没了，输出也读完了
                    None => break,
                }
            }
            let _ = tx.send(SessionEvent::Ended { id, info: exit }).await;
            reap(&weak, id);
        });
        Ok(record)
    }

    /// 远端 shell：先向池要 lease（命中就不碰凭据），再在这条连接上开一条通道。
    pub async fn open_remote(&self, key: TransportKey, term: String, pty: PtySize, encoding: TerminalEncoding, label: String) -> Result<SessionRecord, SessionError> {
        let lease = self.inner.pool.acquire(key.clone(), LeaseKind::Shell).await?;
        let id = self.inner.alloc_id();
        // start 失败时 lease 随参数一起被 drop，租约当场归还——不会留下「有人在用」的幽灵连接
        let session = Arc::new(ShellSession::start(id, lease, term, pty.clone()).await?);
        let record = SessionRecord {
            id,
            label,
            protocol: Protocol::Ssh,
            live_endpoint: Some(session.endpoint().clone()),
            pty,
            encoding,
            cwd: None,
            transport: Some(key),
        };
        self.inner.insert(record.clone(), Handle::Remote(Arc::clone(&session)));

        let weak = Arc::downgrade(&self.inner);
        let tx = self.inner.hub_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = session.next_event().await {
                let mapped = match event {
                    ShellEvent::Data(bytes) => SessionEvent::Data { id, bytes },
                    ShellEvent::Cwd(path) => {
                        // 目录既要进事件流，也要落到条目上：面板可能是先打开的那个
                        if let Some(inner) = weak.upgrade() {
                            inner.set_cwd(id, &path);
                        }
                        SessionEvent::Cwd { id, path }
                    }
                    ShellEvent::Ended(info) => {
                        let _ = tx.send(SessionEvent::Ended { id, info }).await;
                        break;
                    }
                };
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
            // 先放手再摘条目：顺序反了也不影响结果，但「摘条目」才是租约归还的时刻
            drop(session);
            reap(&weak, id);
        });
        Ok(record)
    }

    /// telnet 会话：一条裸 TCP，不进 transport 池、没有 lease、没有主机密钥。
    ///
    /// 与本地终端同形——拨号即连接，登录发生在流里，所以这一层连「凭据」都不需要，
    /// 天然满足「明文不跨 IPC」。收尾仍走同一条路径：转发任务看到通道结束才摘条目。
    pub async fn open_telnet(&self, host: String, port: u16, pty: PtySize, encoding: TerminalEncoding, label: String) -> Result<SessionRecord, SessionError> {
        let (handle, mut events) = dbhx_telnet::connect(&host, port, &pty).await?;
        let id = self.inner.alloc_id();
        let record = SessionRecord {
            id,
            label,
            protocol: Protocol::Telnet,
            live_endpoint: Some(LiveEndpoint { host, port, username: String::new() }),
            pty,
            encoding,
            cwd: None,
            transport: None,
        };
        self.inner.insert(record.clone(), Handle::Telnet(handle));

        let weak = Arc::downgrade(&self.inner);
        let tx = self.inner.hub_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let mapped = match event {
                    TelnetEvent::Data(bytes) => SessionEvent::Data { id, bytes },
                    // telnet 没有退出码概念：流结束就是一次干净的 Ended
                    TelnetEvent::Closed => {
                        let _ = tx.send(SessionEvent::Ended { id, info: ExitInfo::default() }).await;
                        break;
                    }
                };
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
            reap(&weak, id);
        });
        Ok(record)
    }

    /// 键盘输入。
    pub async fn write(&self, id: SessionId, bytes: &[u8]) -> Result<(), SessionError> {
        // 锁只用来取句柄：`send` 可能因对端阻塞而 await，
        // 抱着 `std::sync::Mutex` 去 await 会把整个注册表钉死。
        let handle = self.inner.handle(id)?;
        match handle {
            Handle::Remote(session) => session.write(bytes).await,
            Handle::Local(local) => local.write(bytes).map_err(SessionError::Pty),
            Handle::Telnet(telnet) => telnet.write(bytes).map_err(SessionError::Telnet),
        }
    }

    /// 尺寸变化。成功后写回条目，前端重开面板时才拿得到最新行列。
    pub async fn resize(&self, id: SessionId, pty: &PtySize) -> Result<(), SessionError> {
        let handle = self.inner.handle(id)?;
        let result = match &handle {
            Handle::Remote(session) => session.resize(pty).await,
            Handle::Local(local) => local.resize(pty).map_err(SessionError::Pty),
            // TelnetHandle 的入参是 (cols, rows)，与 `PtySize { rows, cols }` 反着来，别写顺
            Handle::Telnet(telnet) => telnet.resize(pty.cols, pty.rows).map_err(SessionError::Telnet),
        };
        if result.is_ok() {
            self.inner.set_pty(id, pty);
        }
        result
    }

    /// 关闭一个 tab。连接去留由 lease 计数决定，这里只管通道。
    pub async fn close(&self, id: SessionId) -> Result<(), SessionError> {
        let handle = self.inner.handle(id)?;
        match handle {
            // 关掉通道 ⇒ 读任务收到结束 ⇒ 转发任务摘条目。正常路径不必在这里摘。
            Handle::Remote(session) => session.close().await,
            Handle::Local(local) => {
                local.kill();
                Ok(())
            }
            Handle::Telnet(telnet) => {
                // 发出关闭帧 ⇒ 任务退出 ⇒ 转发任务摘条目，与远端同一条收尾链
                telnet.close();
                Ok(())
            }
        }
    }

    pub fn list(&self) -> Vec<SessionRecord> {
        let map = self.inner.entries();
        let mut records: Vec<SessionRecord> = map.values().map(|e| e.record.clone()).collect();
        records.sort_by_key(|r| r.id);
        records
    }

    pub fn record(&self, id: SessionId) -> Option<SessionRecord> {
        self.inner.entries().get(&id).map(|e| e.record.clone())
    }

    /// 这个 tab 用的是哪条 transport。文件面板据此向池要**同一个连接**上的第二个 lease，
    /// 从而做到「从终端跳过去不用再登录一次」。本地终端返回 `None`。
    pub fn transport_key(&self, id: SessionId) -> Option<TransportKey> {
        self.record(id).and_then(|r| r.transport)
    }

    /// 会话数。诊断用。
    pub fn count(&self) -> usize {
        self.inner.entries().len()
    }
}

impl Inner {
    fn alloc_id(&self) -> SessionId {
        // 从 1 开始：0 留给「还没有会话」的哨兵值，前端可以拿它判空
        SessionId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// 锁中毒时继续用：注册表的最差结果只是某个 tab 的信息旧了一格，不值得 panic。
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Entry>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn insert(&self, record: SessionRecord, handle: Handle) {
        self.entries().insert(record.id, Entry { record, handle });
    }

    /// 取出句柄的副本（`Arc` 克隆或 `LocalHandle` 克隆），锁立刻释放。
    fn handle(&self, id: SessionId) -> Result<Handle, SessionError> {
        let map = self.entries();
        let entry = map.get(&id).ok_or(SessionError::NoSuchSession(id.0))?;
        Ok(match &entry.handle {
            Handle::Remote(session) => Handle::Remote(Arc::clone(session)),
            Handle::Local(local) => Handle::Local(local.clone()),
            Handle::Telnet(telnet) => Handle::Telnet(telnet.clone()),
        })
    }

    fn set_pty(&self, id: SessionId, pty: &PtySize) {
        if let Some(entry) = self.entries().get_mut(&id) {
            entry.record.pty = pty.clone();
        }
    }

    fn set_cwd(&self, id: SessionId, path: &str) {
        if let Some(entry) = self.entries().get_mut(&id) {
            entry.record.cwd = Some(path.to_string());
        }
    }

    fn remove(&self, id: SessionId) -> bool {
        self.entries().remove(&id).is_some()
    }
}

/// 流结束：摘条目，让会话对象随转发任务一起消失。
fn reap(weak: &Weak<Inner>, id: SessionId) {
    match weak.upgrade() {
        Some(inner) => {
            if inner.remove(id) {
                tracing::debug!(?id, "会话结束，条目已摘除");
            }
        }
        // 注册表都没了，条目自然也没了
        None => tracing::debug!(?id, "会话结束，注册表已释放"),
    }
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.inner.entries();
        f.debug_struct("SessionManager").field("sessions", &map.keys().collect::<Vec<_>>()).finish()
    }
}
