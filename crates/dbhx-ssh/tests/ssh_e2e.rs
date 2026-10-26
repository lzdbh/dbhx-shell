//! SSH 连接层的**真实协议**端到端测试。
//!
//! 单元测试只能用 `TrustAll`/假流覆盖到「报文怎么拼」，覆盖不到 russh 的握手、
//! 认证状态机、通道确认这些真正容易出错的地方。这里用进程内的 `dbhx-testsrv`
//! 起一台真服务器，把整条路跑一遍。
//!
//! 每个用例都自带 deadline：握手/通道类的 bug 表现为**挂住**而不是报错，
//! 没有超时的测试在 CI 上就是十分钟的沉默。

use std::time::Duration;

use dbhx_model::{LiveEndpoint, PtySize};
use dbhx_ssh::{Credential, DialOptions, ExecOutput, RejectAll, SshError, SshTransport, TrustAll, Chunk};
use dbhx_testsrv::{TestServer, TestSpec, sample_fs};
use zeroize::Zeroizing;

/// 一次往返的最长等待。本地回环 + 内存 VFS，正常都在几十毫秒内完成。
const BUDGET: Duration = Duration::from_secs(10);

fn endpoint(server: &TestServer) -> LiveEndpoint {
    LiveEndpoint { host: server.host(), port: server.port(), username: "app".to_string() }
}

fn password() -> Credential {
    Credential::Password(Zeroizing::new("dbhx-test".to_string()))
}

/// 拨过去并完成认证。绝大多数用例都从这一步开始，失败信息统一在这里给出。
async fn login(server: &TestServer) -> SshTransport {
    let transport = SshTransport::dial(DialOptions::new(endpoint(server)), Box::new(TrustAll))
        .await
        .expect("握手与主机密钥裁决应当成功");
    transport.authenticate(password()).await.expect("正确口令应当认证通过");
    assert!(transport.is_ready(), "认证通过后连接必须处于 ready");
    transport
}

/// 从 shell 里读到出现 `needle` 为止，返回读到的全部字节。
async fn read_until(
    reader: &mut dbhx_ssh::ShellReader,
    needle: &[u8],
) -> Vec<u8> {
    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(chunk) = tokio::time::timeout(remaining, reader.next_chunk()).await.expect("读取超时：通道可能根本没打通") else {
            panic!("通道提前结束，已读到 {:?}", String::from_utf8_lossy(&acc));
        };
        match chunk {
            Chunk::Stdout(bytes) | Chunk::Stderr(bytes) => acc.extend_from_slice(&bytes),
            other => panic!("shell 里不该出现 {other:?}"),
        }
        if acc.windows(needle.len()).any(|w| w == needle) {
            return acc;
        }
    }
}

fn spec() -> TestSpec {
    TestSpec::new().with_fs(sample_fs())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_password_handshake_opens_a_shell_and_carries_keystrokes() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;

    let shell = transport.open_shell("xterm-256color", &PtySize::default()).await.unwrap();
    let (_writer, mut reader) = shell.into_split();

    // 服务器在 shell_request 里就会推一条带 OSC 7 的提示符
    let first = read_until(&mut reader, b"$ ").await;
    assert!(String::from_utf8_lossy(&first).contains("\x1b]7;file://"), "首条提示符必须带 OSC 7，目录跟随才有原料");

    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_input_comes_back_and_cd_moves_the_reported_directory() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;
    let shell = transport.open_shell("xterm-256color", &PtySize::default()).await.unwrap();
    let (writer, mut reader) = shell.into_split();
    read_until(&mut reader, b"$ ").await;

    writer.write(b"whoami\r").await.unwrap();
    let echoed = read_until(&mut reader, b"ran: whoami").await;
    assert!(String::from_utf8_lossy(&echoed).contains("ran: whoami"));

    writer.write(b"cd /home/app/logs\r").await.unwrap();
    let after_cd = read_until(&mut reader, b"file://dbhx-test/home/app/logs").await;
    // 这就是 SFTP 面板跟随所依赖的那个字节序列，必须原样到达客户端
    assert!(String::from_utf8_lossy(&after_cd).contains("\x1b]7;file://dbhx-test/home/app/logs\x07"));

    writer.close().await.unwrap();
    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wrong_password_is_refused_and_the_attempt_is_visible() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = SshTransport::dial(DialOptions::new(endpoint(&server)), Box::new(TrustAll)).await.unwrap();
    let wrong = Credential::Password(Zeroizing::new("not-it".to_string()));
    let err = transport.authenticate(wrong).await.expect_err("错口令不该通过");
    assert!(matches!(err, SshError::AuthFailed), "应当是明确的认证失败，收到 {err:?}");
    let observed = server.observed();
    assert!(observed.auth_attempts.iter().any(|(user, ok)| user == "app" && !ok), "服务器侧应当记下这次失败尝试");
    assert!(!transport.is_ready(), "认证失败的连接不能自称 ready");

    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_port_is_a_network_fault_not_a_panic() {
    // 故意占一个没人听的端口：分类必须是 Unreachable/Timeout，且不能崩
    let port = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap().port()
    };
    let endpoint = LiveEndpoint { host: "127.0.0.1".to_string(), port, username: "app".to_string() };
    let err = match SshTransport::dial(DialOptions::new(endpoint), Box::new(TrustAll)).await {
        Ok(_) => panic!("端口上没人听，拨号不该成功"),
        Err(err) => err,
    };
    assert!(matches!(err, SshError::Timeout | SshError::Unreachable(_)), "连不上应当归成网络类错误，收到 {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_requests_reach_the_server_with_the_right_geometry() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;
    let shell = transport.open_shell("xterm-256color", &PtySize { rows: 30, cols: 110 }).await.unwrap();
    let (writer, _reader) = shell.into_split();

    writer.resize(&PtySize { rows: 12, cols: 40 }).await.unwrap();
    // window-change 是单向消息，等它落地再断言
    tokio::time::sleep(Duration::from_millis(200)).await;
    let observed = server.observed();
    assert!(observed.resizes.contains(&(110, 30)), "开 shell 时的 pty 尺寸应当先到：{:?}", observed.resizes);
    assert!(observed.resizes.contains(&(40, 12)), "resize 之后服务端看到的必须是新尺寸：{:?}", observed.resizes);

    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_collects_stdout_and_the_exit_status() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;
    let exec = transport.open_exec("pwd").await.unwrap();
    let out: ExecOutput = exec.run_to_end().await.unwrap();
    assert!(out.success(), "exit 0 才算成功");
    assert_eq!(out.stdout_utf8(), "/home/app\n");

    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_known_subsystem_opens_and_an_unknown_one_is_refused() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;

    let stream = transport.open_subsystem("sftp").await.expect("sftp 子系统应当可用");
    drop(stream);
    match transport.open_subsystem("nope").await {
        Ok(_) => panic!("未实现的子系统必须被拒绝"),
        Err(err) => assert!(matches!(err, SshError::RequestRefused { request: "subsystem" }), "拒绝原因应当是 subsystem 请求被驳回，收到 {err:?}"),
    }

    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_client_ends_the_session_state() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;
    let mut updates = transport.state_updates();
    transport.close().await;
    let reason = tokio::time::timeout(BUDGET, dbhx_ssh::wait_until_closed(&mut updates)).await.unwrap();
    assert_eq!(reason, dbhx_ssh::CloseReason::ClientRequested);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_host_key_reads_as_a_key_problem_not_a_network_one() {
    let server = TestServer::start(spec()).await.unwrap();
    // 裁决为拒绝时 russh 交回的是一个笼统的握手错误。那个错误解释不了「为什么连不上」，
    // 必须让本地的 HostKeyRejected 结论盖掉它——否则中间人阻断在 UI 上就变成
    // 「请核对地址与端口」，用户会去改端口而不是去查密钥。
    let err = match SshTransport::dial(DialOptions::new(endpoint(&server)), Box::new(RejectAll)).await {
        Err(err) => err,
        Ok(_) => panic!("一律拒绝的裁决不该把握手放过去"),
    };
    assert!(matches!(err, SshError::HostKeyUnknown), "{err:?}");

    server.shutdown().await;
}

/* ── sudo 提权那条通道 ─────────────────────────────────── */

/// 提权失败的断言入口。`ChannelStream` 没有 `Debug`，所以 `expect_err` 在这里用不了
/// （和 `transport.rs` 里 `dial_err` 同一个处境）；顺手把意外成功的流关掉，
/// 免得服务器侧留一条等着 SFTP 报文的僵尸通道。
async fn elevate_err(transport: &SshTransport, sudo_user: &str, password: &str) -> SshError {
    match transport.open_sftp_elevated(sudo_user, password).await {
        Err(err) => err,
        Ok(stream) => {
            drop(stream);
            panic!("这一次提权本该失败，却交出了流")
        }
    }
}

/// 提权成功的形状：口令作为**数据**到达远端，就绪标记回来后交出一条可用的流。
///
/// 「流可用」这一半在 `dbhx-sftp` 的 e2e 里对着真 SFTP 握手断言（那边才有客户端）；
/// 这里只管连接层：交出来的是流而不是错误，且远端看到的正是我们想让它看到的字节。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_elevated_open_delivers_the_password_as_data_not_as_argv() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;

    let stream = transport
        .open_sftp_elevated("root", "dbhx-test")
        .await
        .expect("口令正确且 sudoers 放行时应当交出流");

    let observed = server.observed();
    assert_eq!(observed.sudo_passwords, vec!["dbhx-test".to_string()], "口令应当作为通道的第一行数据到达");
    let command = observed.execs.iter().find(|c| c.contains("sudo -k -A -u")).expect("提权 exec 应当到达服务器");
    assert!(!command.contains("dbhx-test"), "口令绝不能进命令串（远端进程表里看得见）：{command}");
    assert!(command.contains("-u 'root'"), "提权用户要被引号包住：{command}");

    drop(stream);
    transport.close().await;
    server.shutdown().await;
}

/// sudoers 不放行时必须说清楚是「这台机器不允许」，而不是给一条看不懂的协议错误。
///
/// 这条同时守住了那个关键时序：`exec` 的 `Success` 先到、拒绝的话后到。
/// 客户端要是拿 `Success` 当提权成功，这段拒绝就会当成 SFTP 报文喂进解析器，
/// 用户看到的是一条谁也解释不了的长度字段错误。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_elevated_open_without_sudo_rights_says_the_machine_refused_it() {
    let server = TestServer::start(spec().with_sudo_denied()).await.unwrap();
    let transport = login(&server).await;

    let err = elevate_err(&transport, "root", "dbhx-test").await;
    assert!(matches!(err, SshError::ElevateDenied(_)), "收到 {err:?}");
    // 过一遍 IPC：用户在 UI 上看到的就是这一份，分类与可重试性都必须在这里也对
    let ipc = dbhx_model::IpcError::from(err);
    assert!(ipc.message.contains("不允许") && ipc.message.contains("sudo"), "{}", ipc.message);
    assert!(!ipc.message.contains("sudoers file"), "英文原文不该直接透给用户：{}", ipc.message);
    assert!(!ipc.retryable, "重试同一条 sudo 不会改变远端 sudoers");

    transport.close().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_sudo_password_is_named_as_a_password_problem() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;

    let err = elevate_err(&transport, "root", "not-it").await;
    let SshError::ElevateDenied(message) = err else { panic!("应当是提权被拒，收到 {err:?}") };
    assert!(message.contains("口令不正确"), "要和「机器不放行」分得清清楚楚：{message}");
    assert_eq!(server.observed().sudo_passwords, vec!["not-it".to_string()]);

    transport.close().await;
    server.shutdown().await;
}

/// 空口令与超长口令在本地就拒掉：一次都不该碰服务器。
///
/// 远端对空口令的反应是「`read` 拿到了空行，于是继续等下一条」——症状是卡满
/// 15 秒超时。那种等待既花用户的钱又解释不了自己。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_sudo_password_never_reaches_the_server() {
    let server = TestServer::start(spec()).await.unwrap();
    let transport = login(&server).await;

    let err = elevate_err(&transport, "root", "").await;
    assert!(matches!(err, SshError::InvalidInput(_)), "{err:?}");
    assert!(server.observed().execs.is_empty(), "本地就该拦下来，一条 exec 都不该发出去");

    transport.close().await;
    server.shutdown().await;
}
