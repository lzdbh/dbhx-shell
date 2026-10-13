//! `SshTransport`：一条 SSH 连接的所有者。
//!
//! ## 为什么是 actor
//!
//! russh 的 `client::Handle` **不是 `Clone`**，很多方法要 `&mut self`；更麻烦的是
//! `Handle` 一旦被 drop，它交给会话任务的发送端跟着 drop，整条连接就没了。
//! 于是「谁持有 Handle」必须有一个确定答案：
//!
//! ```text
//! 调用方 ──Request──▶ [actor 任务] ──&mut Handle──▶ russh 会话任务 ──▶ TCP
//!                          ▲
//!                    KeyCheck（密钥交换时要问人）
//! ```
//!
//! actor 独占 `Handle`，请求排队执行。副作用正好是我们想要的：**认证一定先于开通道**，
//! 因为 `Authenticate` 是先入队的那个请求。
//!
//! 一个已知代价：rekey 时会话任务会卡在 `check_server_key` 等裁决，此时正在进行的
//! `open_shell` 也会被拖住（最长 = 请求超时 10 秒）。rekey 默认约 1 小时一次，
//! 且 UI 侧对已建立的通道无影响，接受这个取舍。
//!
//! ## 生命周期
//!
//! 所有 `SshTransport` 句柄 drop ⇒ 请求通道关闭 ⇒ actor 退出 ⇒ `Handle` drop ⇒ 连接断开。
//! 会话层的池只要还留着一条 lease，就会留着句柄，所以 idle park 不会误杀连接。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use dbhx_model::{LiveEndpoint, PtySize};
use russh::ChannelStream;
use russh::client::{Handle, Msg};
use tokio::sync::{mpsc, oneshot, watch};

use crate::auth::{authenticate, Credential};
use crate::channel::{confirm, Exec, Shell, SplitChannel};
use crate::config::DialOptions;
use crate::error::SshError;
use crate::handler::{ClientHandler, CloseReason, KeyCheck, SessionState, SshEvent};
use crate::hostkey::ServerKey;

/// actor 的请求队列长度。开通道是低频操作，8 足够堆出可观测的排队；
/// 真正的并发预算在会话层（lease / SFTP 并发数）里管。
const REQUEST_QUEUE: usize = 8;

/// 主机密钥的裁决权在调用方：vault 里的记录、UI 的对话框、测试里的自动放行。
///
/// 显式 boxed future 是为了让 `Box<dyn HostKeyVerifier>` 能进 actor，
/// 同时把「等用户点按钮」这个无上限的等待写进类型。
pub trait HostKeyVerifier: Send {
    fn verify(
        &mut self,
        endpoint: LiveEndpoint,
        key: ServerKey,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

/// 测试与内网批量导入用。生产路径不该用它——它等于 TOFU。
pub struct TrustAll;

impl HostKeyVerifier for TrustAll {
    fn verify(
        &mut self,
        _endpoint: LiveEndpoint,
        _key: ServerKey,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { true })
    }
}

/// 永远拒绝。用来验证「拒绝之后不会留下一条半开的连接」。
pub struct RejectAll;

impl HostKeyVerifier for RejectAll {
    fn verify(
        &mut self,
        _endpoint: LiveEndpoint,
        _key: ServerKey,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { false })
    }
}

type Reply<T> = oneshot::Sender<Result<T, SshError>>;

enum Request {
    Authenticate { credential: Credential, reply: Reply<()> },
    OpenShell { term: String, pty: PtySize, reply: Reply<Shell> },
    OpenExec { command: String, reply: Reply<Exec> },
    /// 给 dbhx-sftp 用：subsystem 建好后把裸流交出去，之后 SFTP 报文不再过 actor
    OpenSubsystem { name: String, reply: Reply<ChannelStream<Msg>> },
    Shutdown,
}

/// 一条 SSH 连接的句柄。多处共享请 `Arc<Self>`（方法都只要 `&self`）。
pub struct SshTransport {
    endpoint: LiveEndpoint,
    tx: mpsc::Sender<Request>,
    state: watch::Receiver<SessionState>,
    /// 取走一次就没了（见 `take_events`），所以是 Option。
    events: Option<mpsc::UnboundedReceiver<SshEvent>>,
}

impl SshTransport {
    /// TCP + 密钥交换 + 主机密钥裁决。成功后连接处于「已握手、未认证」。
    pub async fn dial(
        options: DialOptions,
        verifier: Box<dyn HostKeyVerifier>,
    ) -> Result<Self, SshError> {
        let endpoint = options.endpoint.clone();
        let (state_tx, mut state_rx) = watch::channel(SessionState::Handshaking);
        let state_tx = Arc::new(state_tx);
        let (event_tx, events) = mpsc::unbounded_channel();
        let (check_tx, check_rx) = mpsc::unbounded_channel();
        let (req_tx, req_rx) = mpsc::channel(REQUEST_QUEUE);

        let handler = ClientHandler::new(endpoint.clone(), Arc::clone(&state_tx), event_tx, check_tx);
        let handle = tokio::time::timeout(
            options.handshake_timeout,
            russh::client::connect(
                options.config(),
                (endpoint.host.as_str(), endpoint.port),
                handler,
            ),
        )
        .await
        .map_err(|_| SshError::Timeout)?
        .map_err(|e| classify_connect_error(&e))?;

        tokio::spawn(actor_loop(
            handle,
            Arc::clone(&state_tx),
            state_rx.clone(),
            check_rx,
            req_rx,
            endpoint.clone(),
            verifier,
        ));

        // 主机密钥必须已经裁完才能继续：等到不再是 Handshaking 为止。
        let mut waiter = state_rx.clone();
        wait_for_state(&mut waiter, |s| !matches!(s, SessionState::Handshaking), options.handshake_timeout)
            .await?;
        match &*state_rx.borrow_and_update() {
            SessionState::KeyExchanged | SessionState::Authenticated => {}
            // 裁决为拒绝 ⇒ 要么是没记录（可确认后重试），要么是不一致（硬阻断）。
            // actor 无法区分这两者，交给调用方：它手上的 verifier 才知道自己为什么说不。
            // 这里统一报 Changed 会让「首次连接」变成不可跳过的错误，所以报 Unknown；
            // Strict 策略（后台复用、无人可问）本来就只会因 Unknown 而失败。
            SessionState::Closed(CloseReason::HostKeyRejected) => return Err(SshError::HostKeyUnknown),
            SessionState::Closed(other) => return Err(SshError::Unreachable(format!("{other:?}"))),
            SessionState::Handshaking => return Err(SshError::Timeout),
        }
        Ok(Self { endpoint, tx: req_tx, state: state_rx, events: Some(events) })
    }

    pub fn endpoint(&self) -> &LiveEndpoint {
        &self.endpoint
    }

    pub fn state(&self) -> SessionState {
        self.state.borrow().clone()
    }

    pub fn is_ready(&self) -> bool {
        matches!(*self.state.borrow(), SessionState::Authenticated)
    }

    /// 状态变化的订阅端。每个 clone 自己记「看到哪了」，互不影响。
    ///
    /// 池子必须走这条路观察连接死亡：等关闭要 `&mut` 接收端，而 transport 进
    /// `Arc` 之后只剩 `&self`。
    pub fn state_updates(&self) -> watch::Receiver<SessionState> {
        self.state.clone()
    }

    /// 取走事件接收端，只能成功一次。
    ///
    /// 做成 take 而不是 `next_event(&mut self)` 的理由同上。取走后必须有人抽干，
    /// 否则 `SshEvent` 在无界队列里堆到内存耗尽；没人取走时发送端会随接收端 drop
    /// 而静默丢事件，那是可接受的行为（横幅只是锦上添花）。
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SshEvent>> {
        self.events.take()
    }

    fn is_closed_now(&self) -> bool {
        self.state.borrow().closed()
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, SshError> {
        if self.is_closed_now() {
            return Err(SshError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).await.map_err(|_| SshError::Closed)?;
        rx.await.map_err(|_| SshError::Closed)?
    }

    pub async fn authenticate(&self, credential: Credential) -> Result<(), SshError> {
        self.call(|reply| Request::Authenticate { credential, reply }).await
    }

    pub async fn open_shell(&self, term: &str, pty: &PtySize) -> Result<Shell, SshError> {
        self.require_authenticated()?;
        self.call(|reply| Request::OpenShell { term: term.to_string(), pty: pty.clone(), reply })
            .await
    }

    pub async fn open_exec(&self, command: &str) -> Result<Exec, SshError> {
        self.require_authenticated()?;
        self.call(|reply| Request::OpenExec { command: command.to_string(), reply }).await
    }

    pub async fn open_subsystem(&self, name: &str) -> Result<ChannelStream<Msg>, SshError> {
        self.require_authenticated()?;
        self.call(|reply| Request::OpenSubsystem { name: name.to_string(), reply }).await
    }

    /// 未认证就开通道属于上层 bug，但必须在这一层就拦住：
    /// 放过去只会得到一个 10 秒后的 Timeout，排查成本高得多。
    fn require_authenticated(&self) -> Result<(), SshError> {
        match *self.state.borrow() {
            SessionState::Authenticated => Ok(()),
            SessionState::Closed(_) => Err(SshError::Closed),
            _ => Err(SshError::Russh(russh::Error::NotAuthenticated)),
        }
    }

    /// 主动关闭。句柄全部 drop 也会走到同一条路径。
    pub async fn close(&self) {
        let _ = self.tx.send(Request::Shutdown).await;
    }
}

/// 等一条连接结束并返回原因。会话层用它把标签页标成 Closed。
///
/// 独立成函数而不是 `SshTransport` 的方法，是因为调用方手上通常只有一个
/// `state_updates()` 的 clone（transport 已经在 `Arc` 里）。发送端被 drop 意味着
/// actor 任务已经退出，连接不复存在，同样算关闭。
pub async fn wait_until_closed(rx: &mut watch::Receiver<SessionState>) -> CloseReason {
    loop {
        if let SessionState::Closed(reason) = rx.borrow_and_update().clone() {
            return reason;
        }
        if rx.changed().await.is_err() {
            return CloseReason::Failed("transport 状态发送端已关闭".into());
        }
    }
}

fn classify_connect_error(e: &russh::Error) -> SshError {    use russh::Error::*;
    // 文案是给运维看的，原文是给日志看的：russh 的 Display 是英文短语，
    // 直接透出既看不懂也不稳定，所以固定成可行动中文 + tracing::debug 留原文。
    let actionable = |hint: &str| {
        tracing::debug!(error = %e, "SSH 握手失败");
        SshError::Unreachable(hint.to_string())
    };
    match e {
        // version 交换失败 / 握手期断线，绝大多数是「这个端口不是 SSH」或端口填错
        Version | KexInit | UnknownAlgo | NoCommonAlgo { .. } | Disconnect => {
            actionable("对端在握手时中断，请确认该端口运行的是 SSH 服务")
        }
        // 签名校验失败是协议层给出的唯一「密钥可能被人换过」信号，不能含糊
        PacketAuth | WrongServerSig => SshError::HostKeyChanged,
        Elapsed(_) => SshError::Timeout,
        IO(io) => SshError::Unreachable(io.to_string()),
        // 其余（Kex / Inconsistent / 压缩协商失败等）都是纯协议层问题。
        // russh 在这些变体上给的是英文短语，透给用户既看不懂也不稳定，只进日志。
        _ => actionable("SSH 握手失败，请核对地址与端口"),
    }
}

async fn wait_for_state(
    rx: &mut watch::Receiver<SessionState>,
    done: impl Fn(&SessionState) -> bool,
    budget: Duration,
) -> Result<(), SshError> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if done(&rx.borrow_and_update()) {
            return Ok(());
        }
        if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
            return Err(SshError::Timeout);
        }
    }
}

async fn actor_loop(
    mut handle: Handle<ClientHandler>,
    state_tx: Arc<watch::Sender<SessionState>>,
    mut state_rx: watch::Receiver<SessionState>,
    mut checks: mpsc::UnboundedReceiver<KeyCheck>,
    mut requests: mpsc::Receiver<Request>,
    endpoint: LiveEndpoint,
    mut verifier: Box<dyn HostKeyVerifier>,
) {
    loop {
        tokio::select! {
            check = checks.recv() => {
                match check {
                    Some(c) => {
                        let trust = verifier.verify(endpoint.clone(), c.key).await;
                        // 对端已消失（会话终止）时 send 返回 Err，没有可救的东西
                        let _ = c.reply.send(trust);
                    }
                    // handler 已 drop：会话任务结束，不会再有密钥交换要裁决
                    None => break,
                }
            }
            request = requests.recv() => {
                match request {
                    Some(req) => {
                        if handle_request(&endpoint, &mut handle, &state_tx, req).await {
                            break;
                        }
                    }
                    // 所有 SshTransport 句柄都 drop 了 ⇒ 连接该结束
                    None => break,
                }
            }
            changed = state_rx.changed() => {
                if changed.is_err() || state_rx.borrow().closed() {
                    break;
                }
            }
        }
    }
    // 尽量发个 disconnect，让对端日志是「客户端正常关闭」而不是 TCP 断裂
    let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
    let _ = state_tx.send(SessionState::Closed(CloseReason::ClientRequested));
}

/// 处理一条请求。返回 `true` 表示 actor 应当退出。
async fn handle_request(
    endpoint: &LiveEndpoint,
    handle: &mut Handle<ClientHandler>,
    state_tx: &Arc<watch::Sender<SessionState>>,
    request: Request,
) -> bool {
    match request {
        Request::Authenticate { credential, reply } => {
            let outcome = authenticate(handle, &endpoint.username, credential).await;
            if outcome.is_ok() {
                let _ = state_tx.send(SessionState::Authenticated);
            }
            let _ = reply.send(outcome);
            false
        }
        Request::OpenShell { term, pty, reply } => {
            let outcome = open_shell(handle, &term, &pty).await;
            let _ = reply.send(outcome.map(Shell));
            false
        }
        Request::OpenExec { command, reply } => {
            let outcome = open_exec(handle, &command).await;
            let _ = reply.send(outcome.map(Exec));
            false
        }
        Request::OpenSubsystem { name, reply } => {
            let _ = reply.send(open_subsystem(handle, &name).await);
            false
        }
        Request::Shutdown => true,
    }
}

async fn open_shell(
    handle: &mut Handle<ClientHandler>,
    term: &str,
    pty: &PtySize,
) -> Result<SplitChannel, SshError> {
    let mut channel = handle.channel_open_session().await?;
    channel
        .request_pty(true, term, u32::from(pty.cols), u32::from(pty.rows), 0, 0, &[])
        .await?;
    confirm(&mut channel, "pty-req").await?;
    channel.request_shell(true).await?;
    confirm(&mut channel, "shell").await?;
    Ok(SplitChannel::new(channel))
}

/// 无 PTY 的 exec：留给 SFTP 之外的探测（`pwd`、版本号）。
/// 注意仍然要 confirm，否则命令根本没跑就开始收数据。
async fn open_exec(
    handle: &mut Handle<ClientHandler>,
    command: &str,
) -> Result<SplitChannel, SshError> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command.as_bytes()).await?;
    confirm(&mut channel, "exec").await?;
    Ok(SplitChannel::new(channel))
}

async fn open_subsystem(
    handle: &mut Handle<ClientHandler>,
    name: &str,
) -> Result<ChannelStream<Msg>, SshError> {
    let mut channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, name).await?;
    // 老服务器（某些 Cisco 跳板）根本没装 sftp-server，这里会是 Failure
    confirm(&mut channel, "subsystem").await?;
    Ok(channel.into_stream())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::{Endpoint, LiveEndpoint};
    use tokio::net::TcpListener;

    fn endpoint(port: u16) -> LiveEndpoint {
        LiveEndpoint::from_endpoint(&Endpoint {
            host: "127.0.0.1".into(),
            port: Some(port),
            username: "nobody".into(),
        })
    }

    /// 绑一个端口再立刻释放，得到一个确定没人监听的端口。
    /// 比写死 1 之类更可靠：Windows 上存在系统保留端口段。
    async fn dead_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    /// `SshTransport` 没有 Debug（里面是 russh 的 Handle），所以不能用 expect_err。
    async fn dial_err(port: u16, budget: Duration) -> SshError {
        let mut opts = DialOptions::new(endpoint(port));
        opts.handshake_timeout = budget;
        match SshTransport::dial(opts, Box::new(TrustAll)).await {
            Ok(_) => panic!("不该连接成功"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn dial_to_a_dead_port_is_unreachable() {
        let err = dial_err(dead_port().await, Duration::from_secs(5)).await;
        assert!(matches!(err, SshError::Unreachable(_) | SshError::Timeout), "{err:?}");
    }

    #[tokio::test]
    async fn dial_to_a_non_ssh_listener_is_unreachable_not_a_timeout() {
        // 端口开着但说的不是 SSH（典型场景：把端口填成了 HTTP/Redis）。
        // 这种情况必须快速失败并给出可行动的文案，而不是让 UI 转圈到握手超时。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            let _ = sock.shutdown().await;
        });

        match dial_err(port, Duration::from_secs(5)).await {
            SshError::Unreachable(m) => {
                assert!(m.contains("协商") || m.contains("version") || m.contains("SSH"), "{m}")
            }
            other => panic!("期望 Unreachable，实际 {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_budget_is_actually_enforced() {
        // accept 但不回 SSH id： russh 会一直等，只有我们的 timeout 能救回来。
        // 这条守住了「UI 不会永远卡在连接中」这个产品前提。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            // 故意不回话；连接句柄在本任务结束时才 drop
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let started = std::time::Instant::now();
        let err = dial_err(port, Duration::from_millis(400)).await;
        assert!(matches!(err, SshError::Timeout), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "超时预算没生效：等了 {:?}", started.elapsed());
        server.abort();
    }
}
