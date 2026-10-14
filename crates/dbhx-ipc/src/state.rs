//! 命令层共享的状态：装配、面板登记表、待用户回答的请求。
//!
//! `Core` 是**唯一**被 Tauri 托管的状态，字段全是 `Arc`/`Mutex`，命令拿到 `&Core`
//! 就能干活；命令本身不写业务，只做「取参数 → 调下层 → 翻译错误」。
//! 这样绝大多数逻辑能在没有 webview 的测试里跑：`Core::for_test` 换一个假拨号器，
//! 其余装配与生产完全一致（同一个凭据来源、同一个面板登记表）。
//!
//! 一条铁律贯穿下面所有类型：**`std::sync::Mutex` 的 guard 绝不跨 `await`**。
//! 需要异步时用「锁内取值、锁外等待」的两段式，编译器也会因为 guard 不是 `Send` 而拦住。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use dbhx_model::{HostId, IpcError};
use dbhx_session::{
    Dialer, Lease, LeaseKind, PlanSource, PoolOptions, SessionManager, TransportKey, TransportPool, ssh_dialer,
};
use dbhx_sftp::{Copier, Decision, Engine, FollowGate, Plan, SftpClient, TransferOptions};
use dbhx_ssh::HostKeyVerifier;
use dbhx_vault::{KeySource, KnownHosts, Vault};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::events::{BusEvent, FollowHold, SftpFollowed, hold_of};

/// 事件总线队列。终端输出是主要流量，队列满了说明 webview 跟不上，
/// 此时应当背压到 SSH 通道（发送端 await），而不是把内存吃掉。
pub const BUS_QUEUE: usize = 512;

/// 传输批次的登记表键：批次 id，由 [`Engine`] 内部递增分配。
pub type BatchId = u32;

/// 一个在跑的批次对外的样子。
///
/// 前端要能把进度条挂回**发起它的那个面板**（同一台主机可能开着别的 tab），
/// 所以登记表除取消凭证外还记发起方连接号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct BatchInfo {
    pub batch: BatchId,
    pub connection: u32,
}

/// 登记表条目：取消凭证 + 发起它的面板。
struct LiveBatch {
    cancel: dbhx_sftp::Cancel,
    connection: u32,
}

/// 用户对后端主动询问的回答。
///
/// `Secret` 承载口令：它刻意不 `Debug` 出正文（见手写 `Debug`），也不进任何事件载荷。
pub enum UserAnswer {
    /// 「信任并保存这台主机的主机密钥」
    Confirmed,
    /// 取消：拨号继续走下去但认证会被拒
    Cancelled,
    /// 私钥口令
    Secret(Zeroizing<String>),
}

impl std::fmt::Debug for UserAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Confirmed => f.write_str("Confirmed"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Secret(_) => f.write_str("Secret(<redacted>)"),
        }
    }
}

/// 后端发起、等前端回答的请求登记表。
///
/// 主机密钥确认与私钥口令共用这一个机制：一条 `request` 号对应一个 `oneshot`。
/// 回答必须**恰好一次**：`oneshot::Sender` 从表里 take 走，第二次回答直接报 NotFound，
/// 免得前端重复点击把口令送进另一个已经放弃等待的拨号。
#[derive(Default)]
pub struct Awaits {
    next: AtomicU32,
    senders: Mutex<HashMap<u32, oneshot::Sender<UserAnswer>>>,
}

impl Awaits {
    /// 登记一次等待，返回 `(请求号, 接收端)`。请求号从 1 开始，0 留给「没有请求」。
    pub fn register(&self) -> (u32, oneshot::Receiver<UserAnswer>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id, tx);
        (id, rx)
    }

    pub fn reply(&self, id: u32, answer: UserAnswer) -> Result<(), IpcError> {
        let tx = self.lock().remove(&id).ok_or_else(|| {
            IpcError::new(
                dbhx_model::ErrorCode::NotFound,
                format!("请求 {id} 已不存在，对话框可能已经关过"),
                false,
            )
        })?;
        // send 失败意味着拨号那头已经不等了（超时或用户取消），回答丢了也没后果
        let _ = tx.send(answer);
        Ok(())
    }

    /// 等待方被丢弃时（比如拨号失败）清掉条目，别让表随失败次数增长。
    pub fn drop_request(&self, id: u32) {
        self.lock().remove(&id);
    }

    pub fn pending(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, oneshot::Sender<UserAnswer>>> {
        self.senders.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 一条 transport 的稳定编号。
///
/// 面板、黑名单、跟随门控都按这个号说话，而不是按 `TransportKey`——键里带 endpoint，
/// 用户改主机时键会变，号不变的话登记表就会指向已经换掉的目标。
/// 因此号**跟着键走**：同一个键复用同一个号，键不再被任何面板持有时销号。
#[derive(Default)]
pub struct ConnIds {
    next: AtomicU32,
    by_key: HashMap<TransportKey, u32>,
    by_id: HashMap<u32, TransportKey>,
}

impl ConnIds {
    pub fn of(&mut self, key: &TransportKey) -> u32 {
        if let Some(id) = self.by_key.get(key) {
            return *id;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.by_key.insert(key.clone(), id);
        self.by_id.insert(id, key.clone());
        id
    }

    pub fn key_of(&self, id: u32) -> Option<&TransportKey> {
        self.by_id.get(&id)
    }

    pub fn forget(&mut self, id: u32) -> Option<TransportKey> {
        match self.by_id.remove(&id) {
            Some(key) => {
                self.by_key.remove(&key);
                Some(key)
            }
            None => None,
        }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// 一个开着的文件面板。
///
/// `lease` 是它存在的意义：面板开着时这条 transport 至少有一个引用，
/// 关掉终端 tab 也不会把连接抽走（idle park 的规则见 `dbhx-session`）。
pub struct Panel {
    pub connection: u32,
    pub client: SftpClient,
    pub lease: Lease,
    pub gate: FollowGate,
    /// 面板当前显示的目录（归一化后的绝对路径）
    pub shown: String,
}

/// SFTP 面板登记表。同一 transport 只有一个面板：两个面板跟同一个终端会互相拽，
/// 而且第二条 SFTP subsystem 通道纯属浪费。
#[derive(Default)]
pub struct Panels {
    entries: HashMap<u32, Panel>,
}

impl Panels {
    pub fn get(&self, connection: u32) -> Option<&Panel> {
        self.entries.get(&connection)
    }

    pub fn get_mut(&mut self, connection: u32) -> Option<&mut Panel> {
        self.entries.get_mut(&connection)
    }

    pub fn contains(&self, connection: u32) -> bool {
        self.entries.contains_key(&connection)
    }

    pub fn insert(&mut self, panel: Panel) -> Option<Panel> {
        self.entries.insert(panel.connection, panel)
    }

    /// 关面板。返回的 `Panel` 被丢弃即归还 lease——断开与否由池的 TTL 决定，
    /// 这里不该有任何「顺手把连接也关掉」的动作。
    pub fn remove(&mut self, connection: u32) -> Option<Panel> {
        self.entries.remove(&connection)
    }

    pub fn ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.entries.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 打开面板的结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct PanelInfo {
    pub connection: u32,
    pub endpoint: dbhx_model::LiveEndpoint,
    pub listing: dbhx_model::DirListing,
    /// 面板是否跟随终端目录；关掉它之后 `SftpCwdChanged` 就不再来了
    pub following: bool,
}

/// 命令层的共享状态。
pub struct Core {
    pub dir: PathBuf,
    pub vault: Arc<Mutex<Vault>>,
    pub known: Arc<Mutex<KnownHosts>>,
    pub pool: Arc<TransportPool>,
    pub sessions: SessionManager,
    pub panels: Mutex<Panels>,
    pub conn_ids: Mutex<ConnIds>,
    /// 进不去的目录。按 `(connection, 归一化路径)` 记账，见 `dbhx-sftp::follow`
    pub blacklist: Mutex<dbhx_sftp::Blacklist>,
    pub awaits: Arc<Awaits>,
    pub engine: Engine,
    pub bus: mpsc::Sender<BusEvent>,
    /// 应用锁开关。UI/授权门，不是加密边界：主密钥仍在进程内，但锁屏期间
    /// 一律拒绝拨号与凭据读写。初值来自 `vault.has_lock_pin()`——存了 PIN 就开机即锁。
    /// 用 `Arc<AtomicBool>` 而非 `Core` 字段，是因为凭据来源闭包在 `Core` 之前就要建好。
    pub locked: Arc<AtomicBool>,
    batches: Mutex<HashMap<BatchId, LiveBatch>>,
    /// 传输进度汇聚队列的发送端，交给 `Engine::start`
    progress: mpsc::Sender<dbhx_model::TransferProgress>,
}

impl Core {
    /// 生产装配：真实 SSH 拨号器。
    pub fn production(dir: PathBuf, bus: mpsc::Sender<BusEvent>) -> Result<Arc<Self>, IpcError> {
        Self::assemble(dir, bus, ssh_dialer())
    }

    /// 测试装配：只换拨号器，凭据来源与登记表走同一条代码路径。
    #[cfg(test)]
    pub fn for_test(
        dir: PathBuf,
        bus: mpsc::Sender<BusEvent>,
        dialer: Dialer,
    ) -> Result<Arc<Self>, IpcError> {
        Self::assemble(dir, bus, dialer)
    }

    fn assemble(dir: PathBuf, bus: mpsc::Sender<BusEvent>, dialer: Dialer) -> Result<Arc<Self>, IpcError> {
        std::fs::create_dir_all(&dir).map_err(|e| IpcError::new(dbhx_model::ErrorCode::Io, e.to_string(), false))?;
        let vault = Vault::open(&dir, KeySource::File(dir.join("master.key")))
            .map_err(|e| IpcError::new(dbhx_model::ErrorCode::Io, e.to_string(), false))?;
        let known = KnownHosts::load(&dir)?;
        let awaits = Arc::new(Awaits::default());
        let (progress_tx, progress_rx) = mpsc::channel::<dbhx_model::TransferProgress>(256);

        // 先只有依赖、还没有 `Core`：池会在拨号线程里回调 PlanSource，
        // 让它反过来抓 `Arc<Core>` 就成环了（Core 持池，池的闭包持 Core）。
        // 锁态在 vault 打开后立刻定：存过 PIN 就以「锁着」启动，避免解锁前有一次静默拨号。
        let locked = Arc::new(AtomicBool::new(vault.has_lock_pin()));
        let vault = Arc::new(Mutex::new(vault));
        let known = Arc::new(Mutex::new(known));
        let plans = vault_plan_source(
            Arc::clone(&vault),
            Arc::clone(&known),
            Arc::clone(&awaits),
            Arc::clone(&locked),
            bus.clone(),
        );
        let pool = Arc::new(TransportPool::new(dialer, plans, PoolOptions::default()));
        let sessions = SessionManager::new(Arc::clone(&pool));

        let core = Arc::new(Self {
            dir,
            vault: Arc::clone(&vault),
            known,
            pool,
            sessions,
            panels: Mutex::new(Panels::default()),
            conn_ids: Mutex::new(ConnIds::default()),
            blacklist: Mutex::new(dbhx_sftp::Blacklist::default()),
            awaits,
            engine: Engine::new(TransferOptions::default()),
            bus,
            locked,
            batches: Mutex::new(HashMap::new()),
            progress: progress_tx,
        });
        spawn_progress_forward(Arc::downgrade(&core), progress_rx);
        Ok(core)
    }

    /// vault 所在目录（主机密钥、known_hosts 都在这）。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 应用锁生效时挡住一切会读凭据或发起连接的命令。
    ///
    /// 这是「意图层」的守门：命令在把参数交给下层之前先问一句，锁着就直接回
    /// `Locked`，让前端弹解锁框而不是拨号失败。已建立的会话不受影响（锁不该抽掉
    /// 正在跑的终端），挡的是「新的读取/连接」。
    pub fn require_unlocked(&self) -> Result<(), IpcError> {
        if self.locked.load(Ordering::Relaxed) {
            return Err(IpcError::new(
                dbhx_model::ErrorCode::Locked,
                "应用已锁定，解锁后才能连接或改动凭据",
                false,
            ));
        }
        Ok(())
    }

    pub fn emit(&self, event: BusEvent) {
        // 总线满了就丢：这里 await 会把命令线程钉住，而事件本来就是「尽力送达」的旁路
        if let Err(e) = self.bus.try_send(event) {
            tracing::debug!(?e, "事件总线投递失败");
        }
    }

    pub async fn emit_async(&self, event: BusEvent) -> Result<(), IpcError> {
        self.bus.send(event).await.map_err(|_| IpcError::internal("事件总线已关闭"))
    }

    /// 摘下面板。返回的 `Panel` 被丢弃即归还 lease——断开与否由池的 TTL 决定，
    /// 这里不该有任何「顺手把连接也关掉」的动作。
    pub fn remove_panel(&self, connection: u32) -> Option<Panel> {
        self.panels.lock().unwrap_or_else(|p| p.into_inner()).remove(connection)
    }

    /// 连接是否已被某个面板持有。传输中的面板不允许被关掉。
    pub fn panel_client(&self, connection: u32) -> Result<SftpClient, IpcError> {
        let panels = self.panels.lock().unwrap_or_else(|p| p.into_inner());
        panels
            .get(connection)
            .map(|p| p.client.clone())
            .ok_or_else(|| not_found_panel(connection))
    }

    /// 面板的客户端 **加上它此刻显示给用户的那个目录**。
    ///
    /// 「新建目录」「改名」这类按名字操作必须按正在显示的目录解析，而不是把裸名字
    /// 直接丢给服务器：SFTP 服务器解析相对路径用的是**会话初始目录**（root 用户就是
    /// `/root`），于是「在 /data/logs 里把 a.txt 改名成 b.txt」会把文件搬到 `/root/b.txt`。
    /// `browse` 早就按 `shown` 拼了，这两个命令是漏网的那两个。
    pub fn panel_at(&self, connection: u32) -> Result<(SftpClient, String), IpcError> {
        let panels = self.panels.lock().unwrap_or_else(|p| p.into_inner());
        panels
            .get(connection)
            .map(|p| (p.client.clone(), p.shown.clone()))
            .ok_or_else(|| not_found_panel(connection))
    }

    /// 让面板重开一条 transport 上的第二个 lease（用于「终端还在、面板先关又开」）。
    pub async fn acquire(&self, key: TransportKey, kind: LeaseKind) -> Result<Lease, dbhx_session::SessionError> {
        self.pool.acquire(key, kind).await
    }

    pub fn host_of(&self, id: &HostId) -> Result<dbhx_model::HostRecord, IpcError> {
        let vault = self.vault.lock().unwrap_or_else(|p| p.into_inner());
        vault
            .host(id)
            .cloned()
            .ok_or_else(|| IpcError::new(dbhx_model::ErrorCode::NotFound, "主机记录不存在", false))
    }

    /// 这条 transport 的连接号（没有就顺便登记一个）。
    ///
    /// 号先于面板存在：拨号失败也保留号，同一台主机重试时前端的黑名单/门控记账才不跳号。
    pub fn conn_id_of(&self, key: &TransportKey) -> u32 {
        self.conn_ids.lock().unwrap_or_else(|p| p.into_inner()).of(key)
    }

    pub fn conn_id_count(&self) -> usize {
        self.conn_ids.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// 起跑一批传输，返回批次 id。
    ///
    /// `Batch` 的 `Drop` 就是「取消并中止」，所以它**必须一直活着**：这里把它塞进
    /// 一个跑到结束的任务里，登记表存「取消凭证 + 发起它的面板」。任务结束时顺手摘掉条目、
    /// 放开目录跟随的「传输中」闸门，并发一个批次收尾事件，前端的传输列表据此收敛。
    ///
    /// `connection` 是发起传输的面板：传输期间终端的 `cd` 不该把面板拽走
    /// （参考实现里被投诉过的行为），收尾时才补上期间被压住的那次跳转。
    pub fn start_batch(self: &Arc<Self>, connection: u32, copier: Arc<dyn Copier>, plan: Plan) -> BatchId {
        let batch = self.engine.start(copier, plan, self.progress.clone());
        let id = batch.id();
        self.batches.lock().unwrap_or_else(|p| p.into_inner()).insert(
            id,
            LiveBatch { cancel: batch.cancel_handle(), connection },
        );
        let _ = self.set_busy(connection, true);
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let report = batch.joined().await;
            let ended = match report {
                Ok(r) => crate::events::TransferBatchEnded {
                    batch: id,
                    completed: r.completed as u32,
                    failed: r.failed as u32,
                    cancelled: r.cancelled as u32,
                    bytes: r.bytes,
                    error: None,
                },
                Err(e) => crate::events::TransferBatchEnded {
                    batch: id,
                    completed: 0,
                    failed: 0,
                    cancelled: 0,
                    bytes: 0,
                    error: Some(e.to_string()),
                },
            };
            core.batches.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
            core.emit(BusEvent::TransferBatchEnded(ended));
            // 闸门放开后门控可能立刻给出「期间终端 cd 去了别处」的导航
            match core.set_busy(connection, false) {
                Ok(decision) => {
                    let followed = core.apply_decision(connection, decision).await;
                    if followed.listing.is_some() {
                        core.emit(BusEvent::SftpFollowed(followed));
                    }
                }
                Err(_) => {}
            }
        });
        id
    }

    /// 取消一批传输。已经在跑的分块会在这之后的一次边界上停下。
    pub fn cancel_batch(&self, id: BatchId) -> Result<(), IpcError> {
        let mut batches = self.batches.lock().unwrap_or_else(|p| p.into_inner());
        match batches.remove(&id) {
            Some(entry) => {
                entry.cancel.cancel();
                Ok(())
            }
            None => Err(IpcError::new(
                dbhx_model::ErrorCode::NotFound,
                format!("批次 {id} 已经结束或不存在"),
                false,
            )),
        }
    }

    /// 正在跑的批次。前端重开传输列表时取一次，之后靠 `transfer-progress` /
    /// `transfer-batch-ended` 事件收敛，不需要轮询。
    pub fn running_batches(&self) -> Vec<BatchInfo> {
        let mut infos: Vec<BatchInfo> = self
            .batches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, live)| BatchInfo { batch: *id, connection: live.connection })
            .collect();
        infos.sort_unstable_by_key(|b| b.batch);
        infos
    }

    /// 这些面板里有在跑的传输。关面板前问一句，避免用户把进度条连同面板一起关掉。
    pub fn panels_with_batch(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .batches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|live| live.connection)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// 关面板时清掉这条连接在黑名单里的记账。
    ///
    /// 连接号会被复用（见 `ConnIds`），不清的话新机器的某个目录会莫名「跳不动」。
    pub fn clear_blacklist(&self, connection: u32) {
        self.blacklist.lock().unwrap_or_else(|p| p.into_inner()).clear_connection(connection);
    }

    /// 销号。面板关掉且没有传输在跑时调用。
    pub fn forget_conn(&self, connection: u32) {
        self.conn_ids.lock().unwrap_or_else(|p| p.into_inner()).forget(connection);
    }

    /// 面板的 `Lister`：目录列举走这条通道的同一个 client 克隆。
    pub fn panel_lister(&self, connection: u32) -> Result<dbhx_sftp::Lister, IpcError> {
        let client = self.panel_client(connection)?;
        Ok(Box::new(move |dir: String| {
            let client = client.clone();
            Box::pin(async move { client.read_dir(&dir).await })
        }))
    }

    /// 面板当前展示的目录（绝对、已归一化）。相对路径的命令都按它解析。
    pub fn panel_shown(&self, connection: u32) -> Option<String> {
        let panels = self.panels.lock().unwrap_or_else(|p| p.into_inner());
        panels.get(connection).map(|p| p.shown.clone())
    }

    /// 改一下门控状态并立即求值。
    ///
    /// 锁的次序全应用固定为「面板 → 黑名单」（`hub` 与传输收尾都走这一条），
    /// 有一处反过来就是死锁的素材。
    pub fn decide(
        &self,
        connection: u32,
        f: impl FnOnce(&mut FollowGate, &dbhx_sftp::Blacklist) -> Decision,
    ) -> Result<Decision, IpcError> {
        let mut panels = self.panels.lock().unwrap_or_else(|p| p.into_inner());
        let panel = panels.get_mut(connection).ok_or_else(|| not_found_panel(connection))?;
        let blacklist = self.blacklist.lock().unwrap_or_else(|p| p.into_inner());
        Ok(f(&mut panel.gate, &blacklist))
    }

    /// 传输中闸门。
    pub fn set_busy(&self, connection: u32, busy: bool) -> Result<Decision, IpcError> {
        self.decide(connection, |gate, blacklist| gate.set_busy(busy, blacklist))
    }

    /// 执行门控的决定：该跳就跳，跳不动就拉黑。
    ///
    /// 这是「导航」唯一的落地处——终端回声（`hub`）、用户开关（`commands::sftp`）、
    /// 传输收尾（`start_batch`）都走它，于是「面板显示的目录」与「门控记账的目录」
    /// 不可能被两条路径改成不一样的值。
    /// 返回值 `listing` 有值表示已经换了目录，`hold` 有值表示停在原处并说明原因。
    pub async fn apply_decision(&self, connection: u32, decision: Decision) -> SftpFollowed {
        let stopped = |hold: Option<FollowHold>| SftpFollowed { connection, listing: None, hold };
        let target = match decision {
            Decision::Navigate(target) => target,
            Decision::Hold(hold) => return stopped(hold_of(hold)),
        };
        let Ok(client) = self.panel_client(connection) else {
            // 面板刚被关掉：没什么可跳的，也不该报错——这是一次合法的竞态
            return stopped(None);
        };
        match client.browse(&target).await {
            Ok(listing) => {
                let mut panels = self.panels.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(panel) = panels.get_mut(connection) {
                    panel.gate.confirm_navigated(&listing.path);
                    panel.shown = listing.path.clone();
                }
                SftpFollowed { connection, listing: Some(listing), hold: None }
            }
            Err(e) => {
                tracing::debug!(%target, ?e, "目录导航失败，已拉黑");
                let hold = {
                    let mut panels = self.panels.lock().unwrap_or_else(|p| p.into_inner());
                    let mut blacklist = self.blacklist.lock().unwrap_or_else(|p| p.into_inner());
                    match panels.get_mut(connection) {
                        // 拉黑后目标已从账本里丢掉，门控只会给出 Hold
                        Some(panel) => match panel.gate.mark_unreachable(&mut blacklist, &target) {
                            Decision::Hold(hold) => hold_of(hold),
                            Decision::Navigate(again) => {
                                tracing::warn!(%again, "拉黑后门控仍要求导航");
                                None
                            }
                        },
                        None => None,
                    }
                };
                stopped(hold)
            }
        }
    }
}

pub(crate) fn not_found_panel(connection: u32) -> IpcError {
    IpcError::new(
        dbhx_model::ErrorCode::NotFound,
        format!("面板 {connection} 未打开，请重新从终端跳转"),
        false,
    )
}

/// 生产用的凭据来源。见模块里 `Core::assemble` 的说明：它只吃依赖，不吃 `Core`。
fn vault_plan_source(
    vault: Arc<Mutex<Vault>>,
    known: Arc<Mutex<KnownHosts>>,
    awaits: Arc<Awaits>,
    locked: Arc<AtomicBool>,
    bus: mpsc::Sender<BusEvent>,
) -> PlanSource {
    Box::new(move |key: TransportKey| {
        let (vault, known, awaits, locked, bus) =
            (Arc::clone(&vault), Arc::clone(&known), Arc::clone(&awaits), Arc::clone(&locked), bus.clone());
        Box::pin(async move { build_plan(&vault, &known, &awaits, &locked, &bus, key).await })
    })
}

/// 从 vault 拼出一次拨号的材料。
///
/// 明文只在这段里出现：先 `with_secret` 借出、立刻复制进 `Zeroizing`，
/// 然后**释放 vault 锁**才去等用户输口令（等待无上限，抱着锁等会把整个应用钉死）。
async fn build_plan(
    vault: &Arc<Mutex<Vault>>,
    known: &Arc<Mutex<KnownHosts>>,
    awaits: &Arc<Awaits>,
    locked: &AtomicBool,
    bus: &mpsc::Sender<BusEvent>,
    key: TransportKey,
) -> Result<dbhx_session::DialPlan, dbhx_session::SessionError> {
    use dbhx_session::SessionError;

    // 最后一道防线：命令层已经拦过一次，这里再拦是因为池可能在命令之外
    // （重连、租约补拨）触发 build_plan，锁态必须以进程内原子值为准。
    if locked.load(Ordering::Relaxed) {
        return Err(SessionError::Locked);
    }

    let label = key.describe();
    // 锁内只取值
    let borrowed = {
        let guard = vault.lock().unwrap_or_else(|p| p.into_inner());
        let host = guard.host(&HostId(key.host_id.0.clone())).ok_or_else(|| SessionError::UnknownHost(label.clone()))?;
        let host_label = host.label.clone();
        match &key.auth {
            dbhx_model::AuthRef::Agent => Some(Prepared::Agent),
            dbhx_model::AuthRef::Host { .. } => {
                let secret = host
                    .password_secret_id
                    .clone()
                    .ok_or_else(|| SessionError::NoCredential(host_label))?;
                Some(prepare_password(&guard, &secret)?)
            }
            dbhx_model::AuthRef::Identity { identity_id, .. } => {
                let identity = guard
                    .identities()
                    .iter()
                    .find(|i| i.id.0 == *identity_id)
                    .ok_or_else(|| SessionError::NoCredential(host_label))?;
                Some(prepare_key(&guard, identity)?)
            }
        }
    };
    let prepared = borrowed.ok_or_else(|| SessionError::NoCredential(label.clone()))?;

    let credential = match prepared {
        Prepared::Agent => dbhx_ssh::Credential::Agent,
        Prepared::Password(password) => dbhx_ssh::Credential::Password(password),
        Prepared::Key { pem, passphrase, label: identity_label } => {
            let passphrase = match passphrase {
                Some(p) => Some(p),
                // 加密私钥又没存口令：这时才问人，问不到就当用户取消
                None if dbhx_ssh::key_is_encrypted(&pem) => {
                    let answer = ask(bus, awaits, crate::events::BusEvent::PassphraseRequest(crate::events::PassphraseRequest {
                        request: 0, // 号由 ask 填，这里占位
                        identity_label: identity_label.clone(),
                    }))
                    .await;
                    match answer {
                        UserAnswer::Secret(text) => Some(text),
                        _ => return Err(SessionError::NoCredential(identity_label)),
                    }
                }
                None => None,
            };
            // 先判定格式再交出 pem：`Zeroizing` 不 `Copy`，字段简写会把所有权移走
            let is_ppk = looks_like_ppk(&pem);
            dbhx_ssh::Credential::PrivateKey { pem, passphrase, is_ppk }
        }
    };

    let verifier: Box<dyn HostKeyVerifier> = Box::new(crate::plansrc::VaultVerifier::new(
        Arc::clone(known),
        Arc::clone(awaits),
        bus.clone(),
    ));
    Ok(dbhx_session::DialPlan { credential, verifier, keepalive: key.keepalive })
}

enum Prepared {
    Agent,
    Password(Zeroizing<String>),
    Key { pem: Zeroizing<String>, passphrase: Option<Zeroizing<String>>, label: String },
}

fn prepare_password(vault: &Vault, secret: &dbhx_model::SecretId) -> Result<Prepared, dbhx_session::SessionError> {
    let text = vault
        .with_secret(secret, |bytes| {
            String::from_utf8(bytes.to_vec()).map_err(|_| dbhx_vault::VaultError::Malformed("stored password is not utf-8".into()))
        })
        .map_err(|_| dbhx_session::SessionError::Internal("password decode failed".into()))?;
    Ok(Prepared::Password(Zeroizing::new(text)))
}

fn prepare_key(vault: &Vault, identity: &dbhx_model::IdentityRecord) -> Result<Prepared, dbhx_session::SessionError> {
    let pem = vault
        .with_secret(&identity.private_key_secret_id, |bytes| {
            String::from_utf8(bytes.to_vec()).map_err(|_| dbhx_vault::VaultError::Malformed("stored key is not utf-8".into()))
        })
        .map_err(|_| dbhx_session::SessionError::Internal("private key decode failed".into()))?;
    let passphrase = match &identity.passphrase_secret_id {
        Some(id) => Some(
            vault
                .with_secret(id, |bytes| Ok(Zeroizing::new(String::from_utf8_lossy(bytes).into_owned())))
                .map_err(|_| dbhx_session::SessionError::Internal("passphrase decode failed".into()))?,
        ),
        None => None,
    };
    Ok(Prepared::Key { pem: Zeroizing::new(pem), passphrase, label: identity.label.clone() })
}

/// PuTTY 的 `.ppk` 没有统一扩展名可依赖，靠首行标记认。
fn looks_like_ppk(pem: &str) -> bool {
    pem.trim_start().starts_with("PuTTY-User-Key-File-")
}

/// 发一个带请求号的事件，然后等前端回答。
///
/// 请求号必须在发送前生成，否则可能出现「回答比请求先到」——前端是异步的，这个竞态真实存在。
async fn ask(bus: &mpsc::Sender<BusEvent>, awaits: &Awaits, event: BusEvent) -> UserAnswer {
    let (id, rx) = awaits.register();
    let event = with_request_id(event, id);
    if bus.send(event).await.is_err() {
        awaits.drop_request(id);
        return UserAnswer::Cancelled;
    }
    match rx.await {
        Ok(answer) => answer,
        // 发送端被 drop（比如上面 reply 之前 Core 先退了）按取消处理
        Err(_) => UserAnswer::Cancelled,
    }
}

fn with_request_id(event: BusEvent, id: u32) -> BusEvent {
    match event {
        BusEvent::PassphraseRequest(mut e) => {
            e.request = id;
            BusEvent::PassphraseRequest(e)
        }
        BusEvent::HostKeyPending(mut e) => {
            e.request = id;
            BusEvent::HostKeyPending(e)
        }
        other => other,
    }
}

/// 把 `Engine` 的进度队列接回事件总线。单独一个任务是因为引擎不该知道 Tauri 的存在，
/// 也不该知道事件总线的存在——它只管往队列里塞。
///
/// 抓的是 `Weak<Core>`：任务握着强引用的话，`Core` 就永远等不到释放，
/// 而发送端正是 `Core` 的字段，两端互相钉住，退出时谁都不走。
fn spawn_progress_forward(core: std::sync::Weak<Core>, mut rx: mpsc::Receiver<dbhx_model::TransferProgress>) {
    tauri::async_runtime::spawn(async move {
        while let Some(progress) = rx.recv().await {
            let Some(core) = core.upgrade() else { return };
            if core.bus.send(BusEvent::TransferProgress(crate::events::TransferProgressEvent(progress))).await.is_err() {
                break;
            }
        }
        tracing::debug!("传输进度转发任务结束");
    });
}

/// 测试装配的公共入口：换一个**永远不会成功**的拨号器出来。
///
/// 为什么不用假连接：`SubsystemStream` 是 russh 通道的半成品类型，离线造不出来
/// （`dbhx-session/tests/manager_hub.rs` 里同样的注释）。所以命令层的测试覆盖的是
/// 「注册表 + 门控 + vault + 错误翻译」这一段，真正打通道的路径留给进程内测试服务器。
#[cfg(test)]
pub fn fake_dialer() -> Dialer {
    use dbhx_session::{BoxFuture, DialPlan, SessionError, SharedConnection};
    // 返回值必须显式标注成 BoxFuture，否则推导不出 `dyn Future` 那层间接
    Box::new(|_key, _plan: DialPlan| -> BoxFuture<'static, Result<SharedConnection, SessionError>> {
        Box::pin(async { Err(SessionError::Closed) })
    })
}

/// 起一套与生产同构的 `Core`，并握着事件队列的接收端。
///
/// 返回 `Receiver<BusEvent>` 而不是丢弃它：总线需要有读者，测试靠它断言「发了哪件事」。
#[cfg(test)]
pub fn tests_harness(tag: &str, dialer: Dialer) -> (Arc<Core>, mpsc::Receiver<BusEvent>) {
    let (tx, rx) = mpsc::channel(64);
    let core = Core::for_test(test_dir(tag), tx, dialer).expect("测试装配 Core 失败");
    (core, rx)
}

/// 每个用例一个目录，且先清空——vault 文档会跨用例残留，让「列表里该有几条主机」这种断言失效。
#[cfg(test)]
fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbhx-ipc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_ids_are_stable_per_key_and_reusable_after_forget() {
        let key = sample_key("db1");
        let mut ids = ConnIds::default();
        let first = ids.of(&key);
        assert_eq!(ids.of(&key), first, "同一个键必须是同一个号，否则黑名单会指向别处");
        assert_eq!(ids.key_of(first), Some(&key));

        let other = sample_key("db2");
        assert_ne!(ids.of(&other), first);
        assert_eq!(ids.len(), 2);

        ids.forget(first);
        assert!(ids.key_of(first).is_none(), "销号后不能再反查到键");
        assert_eq!(ids.of(&key), ids.of(&key), "重新登记仍要给出同一个号");
    }

    #[tokio::test]
    async fn awaiting_a_reply_happens_exactly_once() {
        let awaits = Awaits::default();
        let (id, rx) = awaits.register();
        assert_eq!(awaits.pending(), 1);

        awaits.reply(id, UserAnswer::Confirmed).unwrap();
        assert!(rx.await.unwrap().matches_confirmed());
        assert_eq!(awaits.pending(), 0);

        // 第二次回答同一个号必须失败，否则重复点击会把口令送给已经放弃的拨号
        let err = awaits.reply(id, UserAnswer::Cancelled).unwrap_err();
        assert_eq!(err.code, dbhx_model::ErrorCode::NotFound);
    }

    #[test]
    fn dropping_the_receiver_lets_the_entry_be_cleaned_up() {
        let awaits = Awaits::default();
        let (id, rx) = awaits.register();
        drop(rx);
        awaits.drop_request(id);
        assert_eq!(awaits.pending(), 0, "失败路径也要清表，否则表随失败次数增长");
    }

    #[test]
    fn answer_debug_never_prints_the_passphrase() {
        let answer = UserAnswer::Secret(Zeroizing::new("hunter2".into()));
        let text = format!("{answer:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("redacted"), "{text}");
    }

    #[test]
    fn ppk_detection_uses_the_first_line_marker() {
        assert!(looks_like_ppk("PuTTY-User-Key-File-3: ssh-ed25519\nother"));
        assert!(looks_like_ppk("  PuTTY-User-Key-File-2: ssh-rsa\n"));
        assert!(!looks_like_ppk("-----BEGIN OPENSSH PRIVATE KEY-----\n"));
    }

    #[test]
    fn request_id_is_filled_before_the_event_leaves() {
        let event = BusEvent::PassphraseRequest(crate::events::PassphraseRequest { request: 0, identity_label: "k".into() });
        match with_request_id(event, 41) {
            BusEvent::PassphraseRequest(e) => assert_eq!(e.request, 41),
            _ => panic!("请求号填错对象"),
        }
    }

    fn sample_key(host: &str) -> TransportKey {
        TransportKey::new(
            HostId("h1".into()),
            dbhx_model::LiveEndpoint { host: host.into(), port: 22, username: "app".into() },
            dbhx_model::AuthRef::Agent,
            0,
        )
    }

    trait Confirmed {
        fn matches_confirmed(&self) -> bool;
    }
    impl Confirmed for UserAnswer {
        fn matches_confirmed(&self) -> bool {
            matches!(self, Self::Confirmed)
        }
    }
}
