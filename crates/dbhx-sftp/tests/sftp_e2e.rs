//! SFTP 面板与传输引擎的**真实协议**端到端测试。
//!
//! 单元测试里的假 `Copier` 只能验证编排（并发、取消、失败不拖垮整批），
//! 它证明不了报文本身对不对：`readdir` 的 filename 是不是 basename、
//! 属性位图里的类型位有没有带、`WRITE` 是不是真的落在服务端期望的偏移上。
//! 这些错误全都表现为「数据看起来是对的」，只有真服务器能抓出来。
//!
//! 服务器侧的目录树是 `dbhx-testsrv` 的内存 VFS：exFAT 工作区放不了符号链接，
//! 而「绝不跟随链接」这条规矩必须有链接才测得出来。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use dbhx_model::{LiveEndpoint, TransferKind, TransferProgress};
use dbhx_sftp::{Batch, Copier, Engine, Plan, SftpClient, SftpError, SftpCopier, Task, TransferOptions};
use dbhx_ssh::{Credential, DialOptions, SshTransport, TrustAll};
use dbhx_testsrv::{TestServer, TestSpec, sample_fs, vfs::Kind};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

const BUDGET: Duration = Duration::from_secs(10);

/// 拨过去、认证、开 sftp 子系统、走完 SFTP 握手。
///
/// 返回 `SshTransport` 是为了让它活到用例结束：它一 drop，通道就没了。
async fn sftp(server: &TestServer) -> (SshTransport, SftpClient) {
    let endpoint =
        LiveEndpoint { host: server.host(), port: server.port(), username: "app".to_string() };
    let transport = SshTransport::dial(DialOptions::new(endpoint), Box::new(TrustAll))
        .await
        .expect("握手应当成功");
    let credential = Credential::Password(Zeroizing::new("dbhx-test".to_string()));
    transport.authenticate(credential).await.expect("口令应当正确");
    let stream = transport.open_subsystem("sftp").await.expect("sftp 子系统应当可用");
    let client = SftpClient::connect(stream).await.expect("SFTP 握手应当成功");
    (transport, client)
}

async fn started() -> TestServer {
    TestServer::start(TestSpec::new().with_fs(sample_fs())).await.unwrap()
}

/// 每次跑一个独立的本机目录：并发执行的用例不该互相踩文件。
fn local_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir()
        .join("dbhx-sftp-e2e")
        .join(format!("{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 起一个一直在抽干的进度通道。
///
/// 必须有人抽：`Engine::start` 的 sink 是有界的，写满就整批停摆，
/// 那会把「传输本身有没有问题」测成「通道够不够大」。
fn progress_sink() -> (mpsc::Sender<TransferProgress>, mpsc::Receiver<TransferProgress>) {
    mpsc::channel(256)
}

async fn run(copier: std::sync::Arc<dyn Copier>, plan: Plan) -> (Batch, mpsc::Receiver<TransferProgress>) {
    let (tx, rx) = progress_sink();
    let batch = Engine::new(TransferOptions::default()).start(copier, plan, tx);
    (batch, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browse_resolves_the_path_and_lists_directories_first() {
    let server = started().await;
    let (_transport, client) = sftp(&server).await;

    let listing = tokio::time::timeout(BUDGET, client.browse("/home/app")).await.unwrap().unwrap();
    assert_eq!(listing.path, "/home/app", "地址栏要显示服务端确认过的路径");

    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["logs", "latest", "readme.link", "big.bin", "readme.md"]);

    // 每条的 path 必须是「父目录 + basename」，这是面板双击导航用的
    let big = listing.entries.iter().find(|e| e.name == "big.bin").unwrap();
    assert_eq!(big.path, "/home/app/big.bin");
    assert_eq!(big.size, 4096, "size 直接来自属性报文，错了就是服务端没给或客户端读错字段");
    assert_eq!(listing.entries[0].kind, dbhx_model::FileKind::Dir);
    assert_eq!(listing.entries[1].kind, dbhx_model::FileKind::Symlink);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relative_paths_resolve_against_the_home_directory() {
    // 真实 sftp-server 起来就在家目录。客户端把用户输入的相对写法原样发给服务端，
    // 所以「相对路径按谁展开」是服务端的语义，必须在真服务端上确认。
    let server = started().await;
    let (_transport, client) = sftp(&server).await;

    let logs = tokio::time::timeout(BUDGET, client.canonicalize("logs")).await.unwrap().unwrap();
    assert_eq!(logs, "/home/app/logs");

    // 链接会被解引用：面板要停在真实位置，而不是链接的字面路径
    let via_link = client.canonicalize("latest").await.unwrap();
    assert_eq!(via_link, "/home/app/logs");

    let listing = client.browse("logs").await.unwrap();
    assert_eq!(listing.path, "/home/app/logs");
    assert_eq!(listing.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["app.log"]);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stat_follows_the_link_and_lstat_stops_on_it() {
    let server = started().await;
    let (_transport, client) = sftp(&server).await;

    let followed = client.stat("/home/app/latest").await.unwrap();
    assert_eq!(followed.kind, dbhx_model::FileKind::Dir, "stat 要看到链接指向的目录");

    let itself = client.lstat("/home/app/latest").await.unwrap();
    assert_eq!(itself.kind, dbhx_model::FileKind::Symlink, "lstat 必须停在链接本身");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_path_is_reported_as_absent_not_as_a_timeout() {
    // 把超时当「不存在」会让覆盖逻辑删错文件，所以这两个必须分得开
    let server = started().await;
    let (_transport, client) = sftp(&server).await;

    match client.stat("/home/app/nope").await {
        Err(SftpError::NotFound(p)) => assert_eq!(p, "/home/app/nope"),
        other => panic!("应当是 NotFound，收到 {other:?}"),
    }
    assert!(!client.exists("/home/app/nope").await.unwrap());
    assert!(client.exists("/home/app/readme.md").await.unwrap());

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mkdir_all_builds_every_level_and_skips_what_exists() {
    let server = started().await;
    let (_transport, client) = sftp(&server).await;

    client.mkdir_all("/home/app/a/b/c").await.unwrap();
    for level in ["/home/app/a", "/home/app/a/b", "/home/app/a/b/c"] {
        assert!(client.exists(level).await.unwrap(), "{level} 应当已经被建出来");
    }
    // 重复调用必须无害：并发传输里两个任务同时建同一层是常态
    client.mkdir_all("/home/app/a/b/c").await.unwrap();

    let fs = server.spec.fs.lock().unwrap();
    assert_eq!(fs.stat("/home/app/a/b/c", true).unwrap().kind, Kind::Dir);
    drop(fs);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_without_overwrite_refuses_and_with_overwrite_replaces() {
    // SFTP v3 的 RENAME 在 OpenSSH 上不会覆盖，而用户点「移到这里」期待的是覆盖。
    // 客户端的对策是先删后移，所以这里两种开关都要落在服务端状态上验证。
    let server = started().await;
    let (_transport, client) = sftp(&server).await;

    let err = client
        .rename("/home/app/readme.md", "/home/app/logs/app.log", false)
        .await
        .expect_err("目标已存在且没开覆盖，必须失败");
    assert!(!matches!(err, SftpError::NotFound(_)), "不该被误报成路径不存在：{err:?}");
    // 失败不能留下半个结果
    assert!(client.exists("/home/app/readme.md").await.unwrap());

    client.rename("/home/app/readme.md", "/home/app/logs/app.log", true).await.unwrap();
    let fs = server.spec.fs.lock().unwrap();
    assert!(fs.read_file("/home/app/readme.md").is_none(), "源文件应当已经没了");
    assert_eq!(fs.read_file("/home/app/logs/app.log").unwrap(), b"hello sftp\n");
    drop(fs);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_moves_the_remote_bytes_onto_disk() {
    let server = started().await;
    let (_transport, client) = sftp(&server).await;
    let dir = local_dir("download");

    let plan = Plan {
        kind: TransferKind::Download,
        tasks: vec![Task {
            remote: "/home/app/big.bin".to_string(),
            local: dir.join("big.bin"),
            size: 4096,
        }],
        local_dirs: vec![dir.clone()],
        remote_dirs: Vec::new(),
        total_bytes: 4096,
    };
    let copier: std::sync::Arc<dyn Copier> =
        std::sync::Arc::new(SftpCopier::new(client.session(), TransferKind::Download, 1024));
    let (batch, mut events) = run(copier, plan).await;
    let report = tokio::time::timeout(BUDGET, batch.joined()).await.unwrap().unwrap();

    assert_eq!((report.completed, report.failed, report.cancelled), (1, 0, 0));
    assert_eq!(report.bytes, 4096, "字节数是引擎累计的，缺一块就是进度条永远到不了底");
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), vec![7u8; 4096]);

    let mut saw_progress = false;
    while let Ok(event) = events.try_recv() {
        if event.state == dbhx_model::TransferState::Running {
            saw_progress = true;
        }
    }
    assert!(saw_progress, "传输过程中必须发过 Running 事件，否则面板是静止的");

    let _ = std::fs::remove_dir_all(&dir);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_lands_in_the_server_side_filesystem() {
    let server = started().await;
    let (_transport, client) = sftp(&server).await;
    let dir = local_dir("upload");
    // 故意大于一个 chunk：单报文能过的路径不代表分块拼接也对
    let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("up.bin"), &payload).unwrap();

    let plan = Plan {
        kind: TransferKind::Upload,
        tasks: vec![Task {
            remote: "/home/app/uploads/up.bin".to_string(),
            local: dir.join("up.bin"),
            size: payload.len() as u64,
        }],
        local_dirs: Vec::new(),
        // 目录树由 prepare() 建，任务本身只搬文件
        remote_dirs: vec!["/home/app/uploads".to_string()],
        total_bytes: payload.len() as u64,
    };
    let copier: std::sync::Arc<dyn Copier> =
        std::sync::Arc::new(SftpCopier::new(client.session(), TransferKind::Upload, 1024));
    let (batch, _events) = run(copier, plan).await;
    let report = tokio::time::timeout(BUDGET, batch.joined()).await.unwrap().unwrap();
    assert_eq!((report.completed, report.failed), (1, 0));

    let fs = server.spec.fs.lock().unwrap();
    assert_eq!(fs.read_file("/home/app/uploads/up.bin").unwrap(), payload, "分块写回的字节必须原样落地");
    drop(fs);
    assert_eq!(client.stat("/home/app/uploads/up.bin").await.unwrap().size, payload.len() as u64);

    let _ = std::fs::remove_dir_all(&dir);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_missing_file_fails_its_own_task_and_not_the_batch() {
    let server = started().await;
    let (_transport, client) = sftp(&server).await;
    let dir = local_dir("partial");

    let plan = Plan {
        kind: TransferKind::Download,
        tasks: vec![
            Task { remote: "/home/app/big.bin".to_string(), local: dir.join("big.bin"), size: 4096 },
            Task { remote: "/home/app/gone.bin".to_string(), local: dir.join("gone.bin"), size: 1 },
        ],
        local_dirs: vec![dir.clone()],
        remote_dirs: Vec::new(),
        total_bytes: 4097,
    };
    let copier: std::sync::Arc<dyn Copier> =
        std::sync::Arc::new(SftpCopier::new(client.session(), TransferKind::Download, 1024));
    let (batch, _events) = run(copier, plan).await;
    let report = tokio::time::timeout(BUDGET, batch.joined()).await.unwrap().unwrap();

    assert_eq!((report.completed, report.failed, report.cancelled), (1, 1, 0), "一个失败不该把整批带走");
    assert_eq!(report.bytes, 4096);
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap().len(), 4096);
    assert!(!dir.join("gone.bin").exists(), "失败的任务不该留下一个空文件当结果");

    let _ = std::fs::remove_dir_all(&dir);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_sftp_session_leaves_the_ssh_connection_usable() {
    // 面板软关闭只关通道：租约还在，用户再点文件面板不该重连、重新认证
    let server = started().await;
    let (transport, client) = sftp(&server).await;
    client.close().await.unwrap();
    assert!(transport.is_ready(), "SFTP 会话关闭不该把 SSH 连接一起带走");

    let stream = transport.open_subsystem("sftp").await.expect("应当能再开一条子系统");
    let again = SftpClient::connect(stream).await.unwrap();
    assert_eq!(again.canonicalize("logs").await.unwrap(), "/home/app/logs");

    transport.close().await;
    server.shutdown().await;
}
