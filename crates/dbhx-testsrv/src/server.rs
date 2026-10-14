//! 进程内 SSH 服务端：够客户端把「握手 → 认证 → 通道 → 子系统」整条路走一遍。
//!
//! 刻意做得「薄」：shell 不模拟任何真实程序，只是回声 + 一条会随 `cd` 变化的
//! OSC 7 提示符。这不是偷懒——目录跟随的被测点正是**我们从字节流里提取 OSC 7**，
//! 所以这里必须发它，而发什么并不重要。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::server::{Auth, Msg, RunningServerHandle, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;

use crate::sftp::SftpHandler;
use crate::vfs::Vfs;
use crate::HOST_KEY_PEM;

/// 起服务器需要的东西。克隆便宜，多个连接共享同一份 [`Vfs`]。
#[derive(Clone)]
pub struct TestSpec {
    pub username: String,
    pub password: String,
    /// 会话开起来时的当前目录，同时也是 SFTP 相对路径的基准。
    pub home: String,
    pub fs: Arc<Mutex<Vfs>>,
    /// 认证前推送的横幅，用来验证 `SshEvent::AuthBanner` 那条链。
    pub banner: Option<String>,
    /// 公钥认证是否一律接受。关掉它才能测「只有密码对」的分支。
    pub accept_publickey: bool,
}

impl TestSpec {
    /// 用默认口令和一个空根目录起一份配置。
    pub fn new() -> Self {
        Self {
            username: "app".to_string(),
            password: "dbhx-test".to_string(),
            home: "/home/app".to_string(),
            fs: Arc::new(Mutex::new({
                let mut fs = Vfs::new();
                fs.add_dir("/home").add_dir("/home/app");
                fs
            })),
            banner: None,
            accept_publickey: true,
        }
    }

    /// 换掉整棵目录树。测试要的场景（符号链接、大文件、只读目录）由调用方搭。
    pub fn with_fs(mut self, fs: Vfs) -> Self {
        self.fs = Arc::new(Mutex::new(fs));
        self
    }
}

impl Default for TestSpec {
    fn default() -> Self {
        Self::new()
    }
}

/// 服务器观察到的客户端行为，测试从这里取证据而不是猜。
#[derive(Debug, Default, Clone)]
pub struct Observed {
    /// 每次 `window-change` 的 `(列, 行)`
    pub resizes: Vec<(u32, u32)>,
    /// 每一次认证尝试的 `(用户名, 结果)`——包括被拒的那些
    pub auth_attempts: Vec<(String, bool)>,
    /// `exec` 收到的命令行
    pub execs: Vec<String>,
    /// 打开过的子系统名
    pub subsystems: Vec<String>,
}

#[derive(Clone)]
struct Shared {
    spec: TestSpec,
    observed: Arc<Mutex<Observed>>,
}

/// 一条 SSH 会话（一个客户端连接）。
pub struct TestSession {
    shared: Shared,
    /// 会话级状态：通道 id ⇒ 该通道的情形
    channels: HashMap<ChannelId, ChannelState>,
    /// 尚未被 subsystem 取走的原始通道
    pending: HashMap<ChannelId, Channel<Msg>>,
    /// 已交给 SFTP 报文循环的通道。
    ///
    /// russh 在把字节喂给通道的同时**也会**调用 `Handler::data`，
    /// 不认出来的话就会拿 SFTP 报文当终端输入做行回显，两边都会读到脏数据。
    subsystems: HashSet<ChannelId>,
}

struct ChannelState {
    /// 这个 shell 现在的目录，OSC 7 就报它
    cwd: String,
}

impl TestSession {
    fn new(shared: Shared) -> Self {
        Self { shared, channels: HashMap::new(), pending: HashMap::new(), subsystems: HashSet::new() }
    }

    fn note<T>(&self, field: impl FnOnce(&mut Observed) -> T) {
        let mut observed = self.shared.observed.lock().unwrap_or_else(|p| p.into_inner());
        field(&mut observed);
    }
}

/// 登录横幅。形状照真 sshd + pam_motd：先系统欢迎词，再 `Last login`。
///
/// 加它的唯一目的是复现一类客户端 bug——这段字节只在登录那一瞬间写进 pty，
/// 服务端**永远不会重画**。渲染层只要在它到达时还没把终端建起来，
/// 用户看到的就是一个空提示符，而且再也找不回这段信息。
fn motd() -> Vec<u8> {
    "Welcome to Ubuntu 26.04.1 LTS (GNU/Linux 7.0.0-38-generic x86_64)\r\n\
     \r\n\
       * Documentation:  https://docs.ubuntu.com\r\n\
       * Management:     https://landscape.canonical.com\r\n\
     \r\n\
    Last login: Wed Oct 14 12:25:52 2026 from 10.0.0.1\r\n"
        .as_bytes()
        .to_vec()
}

/// 提示符。`OSC 7;file://<host><path>` + BEL 终止，正是 xterm 与主流 shell 的写法。
fn prompt(cwd: &str) -> Vec<u8> {
    format!("\x1b]7;file://dbhx-test{cwd}\x07\r\ndbhx-test:{cwd}$ ").into_bytes()
}

impl russh::server::Handler for TestSession {
    type Error = russh::Error;

    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(self.shared.spec.banner.clone())
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let ok = user == self.shared.spec.username && password == self.shared.spec.password;
        self.note(|o| o.auth_attempts.push((user.to_string(), ok)));
        Ok(if ok { Auth::Accept } else { Auth::Reject { proceed_with_methods: None, partial_success: false } })
    }

    async fn auth_publickey(&mut self, user: &str, _key: &russh::keys::PublicKey) -> Result<Auth, Self::Error> {
        self.note(|o| o.auth_attempts.push((user.to_string(), self.shared.spec.accept_publickey)));
        Ok(if self.shared.spec.accept_publickey { Auth::Accept } else { Auth::Reject { proceed_with_methods: None, partial_success: false } })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), ChannelState { cwd: self.shared.spec.home.clone() });
        self.pending.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.note(|o| o.resizes.push((col_width, row_height)));
        session.channel_success(channel)
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.note(|o| o.resizes.push((col_width, row_height)));
        session.channel_success(channel)
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let cwd = self.channels.get(&channel).map(|c| c.cwd.clone()).unwrap_or_else(|| self.shared.spec.home.clone());
        // 真 sshd 也是这个顺序：横幅先落 pty，再交给 shell 打提示符
        session.data(channel, motd())?;
        session.data(channel, prompt(&cwd))?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        self.note(|o| o.execs.push(command.clone()));
        session.channel_success(channel)?;
        let cwd = self.channels.get(&channel).map(|c| c.cwd.clone()).unwrap_or_else(|| self.shared.spec.home.clone());
        // 只实现目录跟随探针可能用到的那两条，其余命令一律空输出 + 退出码 0
        let output = if command.starts_with("pwd") {
            format!("{cwd}\n")
        } else if let Some(rest) = command.strip_prefix("echo ") {
            format!("{}\n", rest.trim_matches('\''))
        } else {
            String::new()
        };
        session.data(channel, output.into_bytes())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.note(|o| o.subsystems.push(name.to_string()));
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let Some(channel) = self.pending.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        self.subsystems.insert(channel_id);
        let handler = SftpHandler::new(Arc::clone(&self.shared.spec.fs), self.shared.spec.home.clone());
        russh_sftp::server::run(channel.into_stream(), handler).await;
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
        // subsystem 通道的字节是 SFTP 报文，已经由 `russh_sftp::server::run` 在读了。
        // 这里必须一个字都不碰：行回显会把报文塞回通道，客户端的报文解析器
        // 于是看到一堆「长度字段为 1」的垃圾，症状是握手永远等不到应答。
        if self.subsystems.contains(&channel) {
            return Ok(());
        }
        // `\r` 结尾才算一行；没有换行的按键（方向键、Ctrl-C）原样回显，
        // 这样才能测「一次 read 只拿到半截转义序列」那种真实切分。
        let text = String::from_utf8_lossy(data).to_string();
        if !text.ends_with('\r') {
            session.data(channel, data.to_vec())?;
            return Ok(());
        }
        let line = text.trim_end_matches(['\r', '\n']).to_string();
        let cwd = self.channels.get_mut(&channel).map(|c| c.cwd.clone()).unwrap_or_else(|| self.shared.spec.home.clone());
        if line == "exit" {
            self.channels.remove(&channel);
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
        if let Some(target) = line.strip_prefix("cd ") {
            let target = target.trim();
            let next = {
                let fs = self.shared.spec.fs.lock().unwrap_or_else(|p| p.into_inner());
                // `cd` 不带参数回家目录，这条 POSIX 规矩会影响 OSC 7 报出来的路径
                let target = if target.is_empty() { self.shared.spec.home.as_str() } else { target };
                let resolved = fs.resolve(target, &cwd).ok();
                match resolved {
                    Some(path) if fs.is_dir(&path) => Some(path),
                    _ => None,
                }
            };
            match next {
                Some(path) => {
                    if let Some(state) = self.channels.get_mut(&channel) {
                        state.cwd = path;
                    }
                    let shown = self.channels.get(&channel).map(|c| c.cwd.clone()).unwrap_or_default();
                    session.data(channel, prompt(&shown))?;
                    return Ok(());
                }
                None => {
                    session.data(channel, format!("-bash: cd: {target}: No such file or directory\r\n").into_bytes())?;
                    session.data(channel, prompt(&cwd))?;
                    return Ok(());
                }
            }
        }
        // 普通命令：回显一行「ran: <cmd>」再给新提示符，够测试断言「输出确实到了」
        session.data(channel, format!("ran: {line}\r\n").into_bytes())?;
        let shown = self.channels.get(&channel).map(|c| c.cwd.clone()).unwrap_or_default();
        session.data(channel, prompt(&shown))?;
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut ServerSession) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        self.pending.remove(&channel);
        self.subsystems.remove(&channel);
        Ok(())
    }
}

/// russh 要求的 `Server`：每个连接 `new_client` 出一个 [`TestSession`]。
struct Listener {
    shared: Shared,
}

impl russh::server::Server for Listener {
    type Handler = TestSession;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Self::Handler {
        TestSession::new(self.shared.clone())
    }

    fn handle_session_error(&mut self, error: <Self::Handler as russh::server::Handler>::Error) {
        // 客户端中途断开是常态，不该让测试失败
        tracing::debug!(?error, "测试会话结束");
    }
}

/// 起好的服务器。`Drop` 不会自动关，测试结束请调 [`TestServer::shutdown`]。
pub struct TestServer {
    pub addr: SocketAddr,
    pub spec: TestSpec,
    pub observed: Arc<Mutex<Observed>>,
    shutdown: RunningServerHandle,
    join: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// 在 `127.0.0.1:0` 上监听，返回实际端口。
    pub async fn start(spec: TestSpec) -> std::io::Result<Self> {
        Self::start_with(spec, "127.0.0.1:0").await
    }

    pub async fn start_with(spec: TestSpec, bind: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let observed = Arc::new(Mutex::new(Observed::default()));
        let shared = Shared { spec: spec.clone(), observed: Arc::clone(&observed) };

        let host_key = russh::keys::PrivateKey::from_openssh(HOST_KEY_PEM)
            .expect("内置测试主机密钥必须能解析");
        // 认证失败默认要等 3 秒（抗爆破），测试里这就是纯纯的耗时
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let mut server = Listener { shared };
            let running = server.run_on_socket(config, &listener);
            let _ = tx.send(running.handle());
            if let Err(e) = running.await {
                tracing::debug!(?e, "测试服务器已退出");
            }
        });
        let shutdown = rx.await.map_err(|_| std::io::Error::other("测试服务器启动失败"))?;
        Ok(Self { addr, spec, observed, shutdown, join })
    }

    /// 服务器地址，`"127.0.0.1:port"` 形式，直接塞进 `LiveEndpoint`。
    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// 观测记录快照。
    pub fn observed(&self) -> Observed {
        self.observed.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// 停掉 accept 循环并等任务收尾。
    pub async fn shutdown(self) {
        self.shutdown.shutdown("test finished".to_string());
        let _ = self.join.await;
    }
}
