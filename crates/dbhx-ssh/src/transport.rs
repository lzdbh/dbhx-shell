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
use zeroize::Zeroizing;

use crate::auth::{authenticate, Credential};
use crate::channel::{confirm, Exec, Shell, SplitChannel};
use crate::config::DialOptions;
use crate::error::SshError;
use crate::handler::{ClientHandler, CloseReason, KeyCheck, SessionState, SshEvent};
use crate::hostkey::ServerKey;
use crate::sudo_sftp::{elevated_open, validate_inputs};

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
    /// 提权版的裸流：与 `OpenSubsystem` 同为「交出一条 SFTP 线」，但通道里跑的是
    /// 被 sudo 拉起来的 sftp-server，而不是 sshd 自己的 subsystem。
    ///
    /// 口令单独一条字段并包在 `Zeroizing` 里：actor 队列可能积压，
    /// 明文在队列里多躺一秒就多一份可被读出来的副本。
    OpenSftpElevated { sudo_user: String, password: Zeroizing<String>, reply: Reply<ChannelStream<Msg>> },
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
        // 应答任务必须在 connect 之前就位：russh 的 `connect()` 要等密钥交换走完才返回，
        // 而密钥交换又卡在等 `check_server_key` 的回话——先 connect 再 spawn 就是死锁。
        tokio::spawn(verify_loop(check_rx, endpoint.clone(), verifier));
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
        .map_err(|e| {
            // 主机密钥被裁决为「拒绝」时 russh 只能给出一个笼统的握手错误，
            // 那个错误解释不了「为什么连不上」。以本地裁决结论为准：中间人阻断
            // 若被翻译成「请核对地址与端口」，用户会去改端口而不是去查密钥。
            if matches!(&*state_rx.borrow_and_update(), SessionState::Closed(CloseReason::HostKeyRejected)) {
                return SshError::HostKeyUnknown;
            }
            classify_connect_error(&e)
        })?;

        tokio::spawn(actor_loop(
            handle,
            Arc::clone(&state_tx),
            state_rx.clone(),
            req_rx,
            endpoint.clone(),
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

    /// 用当前登录用户的 sudo 口令起一条提权后的 SFTP 线。
    ///
    /// 口令只在这一次调用期间存在，且绝不进 vault、不进日志、不进远端命令串（见
    /// [`crate::sudo_sftp`]）。失败一律原样抛出：调用方不允许「提权不行就用普通身份」，
    /// 那等于骗用户说他看的是 root 的目录。
    ///
    /// 代价写在这里而不是藏起来：actor 是单线程排队，提权最长要等满 `ELEVATE_TIMEOUT`，
    /// 这期间同一条连接上的其它开通道请求会排队。文件面板是低频操作，接受。
    pub async fn open_sftp_elevated(
        &self,
        sudo_user: &str,
        password: &str,
    ) -> Result<ChannelStream<Msg>, SshError> {
        self.require_authenticated()?;
        // 本地检查在入队之前做：空口令不该占用 actor 一次往返，也不该把它的明文
        // 复制进一个马上就会失败的请求里。
        validate_inputs(sudo_user, password)?;
        self.call(|reply| Request::OpenSftpElevated {
            sudo_user: sudo_user.to_string(),
            password: Zeroizing::new(password.to_string()),
            reply,
        })
        .await
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

/// 主机密钥应答任务。
///
/// 独立成一个任务而不是塞进 actor 的 `select!`，是因为它必须在 `connect()` 返回之前
/// 就已经在听；actor 那时还没出生。等待用户点「确认」的那段时间没有上限，
/// 所以这个任务不能持有任何别的锁。
async fn verify_loop(
    mut checks: mpsc::UnboundedReceiver<KeyCheck>,
    endpoint: LiveEndpoint,
    mut verifier: Box<dyn HostKeyVerifier>,
) {
    while let Some(check) = checks.recv().await {
        let trust = verifier.verify(endpoint.clone(), check.key).await;
        // 对端已经消失（会话终止）时 send 返回 Err，没有可救的东西
        let _ = check.reply.send(trust);
    }
}

async fn actor_loop(
    mut handle: Handle<ClientHandler>,
    state_tx: Arc<watch::Sender<SessionState>>,
    mut state_rx: watch::Receiver<SessionState>,
    mut requests: mpsc::Receiver<Request>,
    endpoint: LiveEndpoint,
) {
    loop {
        tokio::select! {
            request = requests.recv() => {
                match request {
                    // 返回值表示「actor 该退出了」：主动关闭的原因必须在
                    // russh 的 `disconnected` 回调之前钉下去，否则那条回调带着
                    // `Error(Disconnected)` 后到，把用户关连接误记成「连接失败」，
                    // UI 会给出不存在的重试建议。
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
                // 必须用 `borrow_and_update`：`borrow` 不推进这个 receiver 的观察版本，
                // 那样每次状态变化之后本分支都会立刻就绪，actor 变成空转的忙等。
                if changed.is_err() || state_rx.borrow_and_update().closed() {
                    break;
                }
            }
        }
    }
    // 先记原因再发 disconnect：disconnect 会让会话任务收尾，
    // handler 的 `disconnected` 回调随时可能抢到写权。
    stamp_closed(&state_tx, CloseReason::ClientRequested);
    // 尽量发个 disconnect，让对端日志是「客户端正常关闭」而不是 TCP 断裂
    let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
}

/// 只在「还没有关闭原因」时写入关闭原因。
///
/// 关闭原因以**第一个**为准：谁先观察到连接结束，谁就是真正的原因。
/// handler 侧的 `set_state` 有同样的规矩，两边加起来才能保证
/// 「服务器断开」不会被随后的本地清理改写成 `ClientRequested`。
fn stamp_closed(state_tx: &watch::Sender<SessionState>, reason: CloseReason) {
    if state_tx.borrow().closed() {
        return;
    }
    let _ = state_tx.send(SessionState::Closed(reason));
}

/// 处理一条请求。返回 `true` 表示连接该结束（只有 `Shutdown` 会）。
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
        Request::OpenSftpElevated { sudo_user, password, reply } => {
            let _ = reply.send(open_sftp_elevated(handle, endpoint, &sudo_user, &password).await);
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
    let mut early = confirm(&mut channel, "pty-req").await?;
    channel.request_shell(true).await?;
    early.extend(confirm(&mut channel, "shell").await?);
    Ok(SplitChannel::with_early(channel, early))
}

/// 无 PTY 的 exec：留给 SFTP 之外的探测（`pwd`、版本号）。
/// 注意仍然要 confirm，否则命令根本没跑就开始收数据。
async fn open_exec(
    handle: &mut Handle<ClientHandler>,
    command: &str,
) -> Result<SplitChannel, SshError> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command.as_bytes()).await?;
    let early = confirm(&mut channel, "exec").await?;
    Ok(SplitChannel::with_early(channel, early))
}

async fn open_subsystem(
    handle: &mut Handle<ClientHandler>,
    name: &str,
) -> Result<ChannelStream<Msg>, SshError> {
    let mut channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, name).await?;
    // 老服务器（某些 Cisco 跳板）根本没装 sftp-server，这里会是 Failure
    // sftp 是纯请求驱动的：服务器在收到第一个报文之前不会写任何字节，
    // 所以确认阶段不可能有输出要接管。真出现了就是协议异常，留一条日志好查。
    let early = confirm(&mut channel, "subsystem").await?;
    if !early.is_empty() {
        tracing::warn!(name, count = early.len(), "子系统确认前收到意外数据，已丢弃");
    }
    Ok(channel.into_stream())
}

/// 提权 SFTP：开一条 session 通道，交进 [`elevated_open`] 走完 pty + exec + sudo 握手。
///
/// 与 `open_subsystem` 分开而不是共用一条路径：subsystem 的确认阶段「不可能有输出」，
/// 提权的确认阶段**必须**接管输出——那些字节正是失败原因的全部证据。混在一起写，
/// 就会被 `open_subsystem` 那句「收到意外数据就丢弃」的错误假设污染。
async fn open_sftp_elevated(
    handle: &mut Handle<ClientHandler>,
    endpoint: &LiveEndpoint,
    sudo_user: &str,
    password: &str,
) -> Result<ChannelStream<Msg>, SshError> {
    let channel = handle.channel_open_session().await?;
    elevated_open(channel, endpoint, sudo_user, password).await
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
