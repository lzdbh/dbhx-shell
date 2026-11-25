//! 池里存放的连接抽象，以及它的 SSH 实现。
//!
//! 为什么是 trait 而不是直接 `Arc<SshTransport>`：池的全部难点（身份键判等、lease 计数、
//! idle park、并发拨号去重）都不依赖 SSH 本身，只有把它们和真实网络隔开，才能在
//! 毫秒级单测里断言「五个并发请求只拨了一次」「TTL 到点真的断开了」。
//! 进程内测试服务器到位后这套测试仍然有用——它测的是池，不是 russh。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use dbhx_model::{LiveEndpoint, PtySize};
use dbhx_ssh::{
    wait_until_closed, CloseReason, DialOptions, Exec, SessionState, Shell, SshError, SshEvent,
    SshTransport, SubsystemStream,
};
use tokio::sync::mpsc::UnboundedReceiver;
use zeroize::Zeroizing;

use crate::error::SessionError;
use crate::key::TransportKey;
use crate::plan::DialPlan;

/// 手写 boxed future：不引 `futures`，trait 里也就不用写 async fn。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 池对外交出的连接句柄。
pub type SharedConnection = Arc<dyn PooledConnection>;

/// 一条可复用、已认证的连接。
///
/// 方法与 `SshTransport` 一一对应，差别只在签名：参数按值、返回 `BoxFuture`，
/// 这样 `Arc<dyn PooledConnection>` 才既能跨任务共享又能被 trait 对象调用。
pub trait PooledConnection: Send + Sync + 'static {
    fn endpoint(&self) -> &LiveEndpoint;

    /// 服务器在认证阶段推的登录横幅（第一条）。
    fn auth_banner(&self) -> Option<String>;

    fn state(&self) -> SessionState;

    fn is_closed(&self) -> bool {
        self.state().closed()
    }

    fn open_shell(&self, term: String, pty: PtySize) -> BoxFuture<'_, Result<Shell, SshError>>;

    fn open_exec(&self, command: String) -> BoxFuture<'_, Result<Exec, SshError>>;

    /// SFTP 面板用：拿到 subsystem 裸流之后，SFTP 报文不再经过池。
    fn open_subsystem(&self, name: String) -> BoxFuture<'_, Result<SubsystemStream, SshError>>;

    /// 提权版 SFTP 裸流：同一条已认证 transport 上，用 `sudo_user` 的身份起一条
    /// 跑在 sudo 里的 sftp-server。口令按值交出并包在 `Zeroizing` 里——它从命令行
    /// 一路到远端 `sudo -A` 之间只活在这条调用栈上，不落 vault、不进日志。
    ///
    /// 失败必须原样往上抛。上层不许写「提权不行就退回普通身份」：用户点的是提权，
    /// 悄悄降级等于让他以为自己在管 root 的文件。
    fn open_sftp_elevated(
        &self,
        sudo_user: String,
        password: Zeroizing<String>,
    ) -> BoxFuture<'_, Result<SubsystemStream, SshError>>;

    /// 本地/动态转发的底层：在这条已认证连接上向服务器要一条通往 `host:port` 的
    /// `direct-tcpip` 通道。目标由**服务器**去连。
    fn open_direct_tcpip(
        &self,
        host: String,
        port: u16,
    ) -> BoxFuture<'_, Result<SubsystemStream, SshError>>;

    /// 让服务器监听 `bind_host:bind_port`，把进来的连接转回本机目标。
    fn remote_forward(
        &self,
        bind_host: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    ) -> BoxFuture<'_, Result<u32, SshError>>;

    /// 主动关闭。淘汰路径必须走它而不能只 drop——supervisor 任务还握着一条句柄，
    /// 光靠 drop 到不了对端。
    fn close(&self) -> BoxFuture<'_, ()>;

    /// 等连接结束。每条连接一个 supervisor 任务，靠它感知远端断开。
    fn wait_closed(&self) -> BoxFuture<'_, CloseReason>;
}

/// 拨号器：握手 + 认证，产出一条可直接放进池的连接。
///
/// 带 `TransportKey` 是为了让实现方有地方写「在拨谁」；拨号目标本身在键和计划里。
pub type Dialer = Box<
    dyn Fn(TransportKey, DialPlan) -> BoxFuture<'static, Result<SharedConnection, SessionError>>
        + Send
        + Sync,
>;

/// 生产用的拨号器。
pub fn ssh_dialer() -> Dialer {
    Box::new(|key, plan| Box::pin(async move { SshConnection::connect(&key, plan).await }))
}

pub struct SshConnection {
    transport: Arc<SshTransport>,
    /// 跳板链上的每条连接。它们必须活到目标连接结束：目标握手用的 `direct-tcpip`
    /// 通道挂在最靠近目标的那一跳上，跳板 transport 一 drop 通道就断了。
    _hops: Vec<Arc<SshTransport>>,
}

impl SshConnection {
    /// 任何一步失败都不留半开连接：此时本地握的是最后一个句柄，它一出作用域 actor
    /// 就退出并给对端发 disconnect。
    pub async fn connect(
        key: &TransportKey,
        plan: DialPlan,
    ) -> Result<SharedConnection, SessionError> {
        let DialPlan {
            credential,
            verifier,
            keepalive,
            hops,
        } = plan;
        let endpoint = key.endpoint.clone();

        // 逐跳建立：第一跳走真实 TCP，之后每一跳都在上一跳的 direct-tcpip 里握手。
        // 上一跳的 transport 留在 `chain` 里，这样通道不会随临时变量一起 drop。
        let mut chain: Vec<Arc<SshTransport>> = Vec::with_capacity(hops.len());
        for hop in hops {
            let mut options = DialOptions::new(hop.endpoint.clone());
            options.keepalive = hop.keepalive;
            let transport = if let Some(prev) = chain.last() {
                let stream = prev
                    .open_direct_tcpip(&hop.endpoint.host, hop.endpoint.port)
                    .await?;
                SshTransport::dial_stream(options, verifier.boxed(), stream).await?
            } else {
                SshTransport::dial(options, verifier.boxed()).await?
            };
            let mut transport = transport;
            if let Some(events) = transport.take_events() {
                tokio::spawn(drain_events(hop.endpoint.host.clone(), events));
            }
            transport.authenticate(hop.credential).await?;
            chain.push(Arc::new(transport));
        }

        let mut options = DialOptions::new(endpoint.clone());
        // 键里说没有 keepalive，配置里就必须真的没有；两处不一致会让身份键骗人
        options.keepalive = keepalive;
        let mut transport = if let Some(prev) = chain.last() {
            let stream = prev
                .open_direct_tcpip(&endpoint.host, endpoint.port)
                .await?;
            SshTransport::dial_stream(options, verifier.boxed(), stream).await?
        } else {
            SshTransport::dial(options, verifier.boxed()).await?
        };
        if let Some(events) = transport.take_events() {
            tokio::spawn(drain_events(endpoint.host.clone(), events));
        }
        transport.authenticate(credential).await?;
        Ok(Arc::new(Self {
            transport: Arc::new(transport),
            _hops: chain,
        }))
    }
}

/// 事件接收端必须有人抽干，否则 `SshEvent` 在无界队列上堆到内存耗尽。
/// M2 还没有消费方（横幅属于认证 UI），先只留痕。
async fn drain_events(host: String, mut events: UnboundedReceiver<SshEvent>) {
    while let Some(event) = events.recv().await {
        tracing::debug!(%host, ?event, "SSH 事件");
    }
}

impl PooledConnection for SshConnection {
    fn endpoint(&self) -> &LiveEndpoint {
        self.transport.endpoint()
    }

    fn auth_banner(&self) -> Option<String> {
        self.transport.auth_banner()
    }

    fn state(&self) -> SessionState {
        self.transport.state()
    }

    fn open_shell(&self, term: String, pty: PtySize) -> BoxFuture<'_, Result<Shell, SshError>> {
        Box::pin(async move { self.transport.open_shell(&term, &pty).await })
    }

    fn open_exec(&self, command: String) -> BoxFuture<'_, Result<Exec, SshError>> {
        Box::pin(async move { self.transport.open_exec(&command).await })
    }

    fn open_subsystem(&self, name: String) -> BoxFuture<'_, Result<SubsystemStream, SshError>> {
        Box::pin(async move { self.transport.open_subsystem(&name).await })
    }

    fn open_sftp_elevated(
        &self,
        sudo_user: String,
        password: Zeroizing<String>,
    ) -> BoxFuture<'_, Result<SubsystemStream, SshError>> {
        Box::pin(async move {
            self.transport
                .open_sftp_elevated(&sudo_user, &password)
                .await
        })
    }

    fn open_direct_tcpip(
        &self,
        host: String,
        port: u16,
    ) -> BoxFuture<'_, Result<SubsystemStream, SshError>> {
        Box::pin(async move { self.transport.open_direct_tcpip(&host, port).await })
    }

    fn remote_forward(
        &self,
        bind_host: String,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    ) -> BoxFuture<'_, Result<u32, SshError>> {
        Box::pin(async move {
            self.transport
                .remote_forward(&bind_host, bind_port, &target_host, target_port)
                .await
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.transport.close())
    }

    fn wait_closed(&self) -> BoxFuture<'_, CloseReason> {
        let mut rx = self.transport.state_updates();
        Box::pin(async move { wait_until_closed(&mut rx).await })
    }
}
