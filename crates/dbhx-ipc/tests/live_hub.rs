//! 命令层对着**真服务器**的端到端用例。
//!
//! `state::tests_harness` 那条路只能用假拨号器：`SubsystemStream` 是 russh 通道的
//! 半成品类型，离线造不出来。于是「拨号 → 认证 → shell 通道 → OSC 7 → 复用同一条
//! transport 开 SFTP → 面板跟随 → 批量下载」这一整串衔接从来没被跑通过——每一段
//! 单测各自绿，接缝处一个都没验证过。这里补的就是接缝：
//!
//! - **免登录复用**：开面板时服务端只能有**一次**认证记录，多一次就是重新登录。
//! - **目录跟随**：终端 `cd` 之后面板要跟上；用户手动浏览过就必须先压住，
//!   重新打开跟随时再补上那次跳转。
//! - **传输收尾**：批次事件的字节数与本机文件内容要一致。
//! - **主机密钥**：TOFU 确认一次就落盘；密钥变更是硬阻断，没有「仍然连接」。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbhx_ipc::commands::log::{start_impl as start_log_impl, stop_impl as stop_log_impl};
use dbhx_ipc::commands::session::{close_impl as close_session_impl, open_remote_impl, write_impl};
use dbhx_ipc::commands::sftp::{browse_impl, chmod_impl, close_impl as close_panel_impl, create_file_impl, mkdir_impl, open_for_session_impl, rename_impl, set_following_impl};
use dbhx_ipc::commands::transfer::download_impl;
use dbhx_ipc::commands::vault::{HostDraft, UserReply, reply_impl, set_password_impl, upsert_host_impl};
use dbhx_ipc::events::{BusEvent, FollowHold, TermData};
use dbhx_ipc::hub;
use dbhx_ipc::state::{BUS_QUEUE, Core};
use dbhx_model::{AuthMode, Endpoint, ErrorCode, FileKind, KnownHost, KnownHostSource, OsType, Protocol, PtySize, TerminalEncoding};
use dbhx_ssh::pattern_of;
use dbhx_testsrv::{HOST_KEY_ALGO, HOST_KEY_B64, TestServer, TestSpec, sample_fs};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// 一次本机往返的预算。真握手在回环上是毫秒级，给到 15 秒只为容忍 CI 上被抢占的线程。
const BUDGET: Duration = Duration::from_secs(15);

/// 起一套与生产同构的装配（真实 SSH 拨号器），并接上唯一的会话事件读者。
fn live_core(tag: &str) -> (Arc<Core>, mpsc::Receiver<BusEvent>) {
    let (tx, rx) = mpsc::channel(BUS_QUEUE);
    let core = Core::production(scratch(tag), tx).expect("Core 装配失败");
    assert!(hub::spawn(Arc::clone(&core)), "会话事件读者只能有一个，起不来说明装配重复");
    (core, rx)
}

/// 用例之间互不踩目录：vault、known_hosts 与下载落盘都会跨用例残留。
fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir()
        .join("dbhx-ipc-live")
        .join(format!("{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 把测试服务器的主机密钥预先写成「已信任」。
///
/// 不这么做的话每次拨号都走「问用户」那条分支，而问答本身正是被测路径的一部分——
/// 让证人自己给自己作证只会把断言变虚。TOFU 分支由
/// `untrusted_host_key_is_asked_once_then_persisted` 单独覆盖。
fn trust_the_server(core: &Arc<Core>, server: &TestServer) {
    let host = KnownHost {
        pattern: pattern_of(&server.host(), server.port()),
        algorithm: HOST_KEY_ALGO.to_string(),
        base64_key: HOST_KEY_B64.to_string(),
        added_at_ms: 0,
        source: KnownHostSource::UserConfirmed,
    };
    core.known.lock().unwrap_or_else(|p| p.into_inner()).trust(host);
}

/// 取出第一个满足条件的事件。
///
/// 途中遇到主机密钥询问就代用户点「信任并保存」：本用例关心的不是这一段，
/// 但拨号会一直等在那里，不把答案送回去只会变成超时。
async fn await_event<F>(core: &Arc<Core>, rx: &mut mpsc::Receiver<BusEvent>, what: &str, pred: F) -> BusEvent
where
    F: Fn(&BusEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("等待「{what}」超时"))
            .unwrap_or_else(|| panic!("事件总线已关闭，等不到「{what}」"));
        if pred(&event) {
            return event;
        }
        if let BusEvent::HostKeyPending(pending) = &event {
            reply_impl(core.as_ref(), pending.request, UserReply::Confirmed).expect("代用户确认主机密钥失败");
        }
    }
}

fn cwd_reported<'a>(path: &'a str) -> impl Fn(&BusEvent) -> bool + 'a {
    move |event| matches!(event, BusEvent::SessionCwd(c) if c.path == path)
}

fn batch_ended(batch: u32) -> impl Fn(&BusEvent) -> bool {
    move |event| matches!(event, BusEvent::TransferBatchEnded(t) if t.batch == batch)
}

fn draft(server: &TestServer) -> HostDraft {
    HostDraft {
        id: None,
        label: "本机测试服务器".into(),
        group: None,
        tags: vec![],
        favorite: false,
        protocol: Protocol::Ssh,
        os: OsType::Linux,
        endpoint: Endpoint { host: server.host(), port: Some(server.port()), username: "app".into() },
        auth_mode: AuthMode::Password,
        identity_id: None,
        keepalive_secs: 0,
        encoding: TerminalEncoding::Utf8,
    }
}

/// 主机记录 + 口令写进 vault；返回主机 id。
fn register_host(core: &Arc<Core>, server: &TestServer) -> dbhx_model::HostId {
    register_with(core, draft(server))
}

/// 按给定草案登记（口令统一用测试口令）。要试「未知系统」这类变体的用例走它。
fn register_with(core: &Arc<Core>, draft: HostDraft) -> dbhx_model::HostId {
    let id = upsert_host_impl(core.as_ref(), draft).unwrap();
    set_password_impl(core.as_ref(), id.clone(), Zeroizing::new("dbhx-test".to_string())).unwrap();
    id
}

async fn started_server() -> TestServer {
    server_running(TestSpec::new().with_fs(sample_fs())).await
}

async fn server_running(spec: TestSpec) -> TestServer {
    TestServer::start(spec).await.expect("测试服务器起不来")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_cd_then_panel_then_download_reuse_one_login() {
    let server = started_server().await;
    let (core, mut rx) = live_core("flow");
    trust_the_server(&core, &server);
    let host_id = register_host(&core, &server);

    let session = open_remote_impl(core.as_ref(), host_id, PtySize { rows: 30, cols: 110 })
        .await
        .expect("远端 shell 应当开起来");

    // 提示符本身就带一次 OSC 7：先确认字节流真的经过了 hub
    await_event(&core, &mut rx, "初始目录", cwd_reported("/home/app")).await;

    write_impl(core.as_ref(), session.id, b"cd /home/app/logs\r".to_vec()).await.unwrap();
    await_event(&core, &mut rx, "终端切换目录", cwd_reported("/home/app/logs")).await;

    // 从终端开面板：复用那条 transport，服务端不该看到第二次认证
    let panel = open_for_session_impl(core.as_ref(), session.id).await.expect("面板应当免登录打开");
    assert_eq!(panel.listing.path, "/home/app/logs", "面板要停在终端报过的目录");
    assert_eq!(server.observed().auth_attempts.len(), 1, "开面板绝不能重新登录");

    let refreshed = browse_impl(core.as_ref(), panel.connection, None).await.unwrap();
    assert_eq!(refreshed.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["app.log"]);

    // 下载：相对路径按面板当前目录解析，收尾事件的字节数要等于文件真实长度
    let dest = scratch("download");
    let batch =
        download_impl(&core, panel.connection, vec!["app.log".to_string()], dest.display().to_string()).await.expect("批次应当起跑");
    let ended = match await_event(&core, &mut rx, "传输收尾", batch_ended(batch)).await {
        BusEvent::TransferBatchEnded(t) => t,
        other => panic!("期望批次收尾事件，收到 {other:?}"),
    };
    assert_eq!((ended.completed, ended.failed, ended.cancelled), (1, 0, 0), "一个文件该成一次");
    assert_eq!(ended.bytes, 14, "收尾字节数错了就是引擎与命令层对不上");
    assert_eq!(ended.error, None, "{:?}", ended.error);
    assert_eq!(std::fs::read(dest.join("app.log")).unwrap(), b"line-1\nline-2\n", "落盘内容必须与服务器一致");

    // 面板还开着，终端再 `cd` 一次。但用户刚才手动浏览过面板，于是这次跳转必须**被压住**——
    // 「点了目录结果自己跳回去」正是参考实现里被投诉过的行为。
    write_impl(core.as_ref(), session.id, b"cd /home/app\r".to_vec()).await.unwrap();
    await_event(&core, &mut rx, "终端切换目录", cwd_reported("/home/app")).await;
    let held = match await_event(&core, &mut rx, "手动浏览后的锁定", |e| {
        matches!(e, BusEvent::SftpFollowed(f) if f.listing.is_none() && f.hold.is_some())
    })
    .await
    {
        BusEvent::SftpFollowed(f) => f.hold,
        other => panic!("期望面板停在原处，收到 {other:?}"),
    };
    assert!(matches!(held, Some(FollowHold::UserLocked)), "{held:?}");
    assert_eq!(core.panel_shown(panel.connection).as_deref(), Some("/home/app/logs"), "锁定期内面板不能动");

    // 用户重新打开跟随 ⇒ 立刻补上期间被压住的那次跳转
    let followed = set_following_impl(core.as_ref(), panel.connection, true).await.unwrap();
    assert_eq!(followed.listing.as_ref().map(|l| l.path.as_str()), Some("/home/app"), "恢复跟随时要补跳");
    assert_eq!(core.panel_shown(panel.connection).as_deref(), Some("/home/app"));

    close_panel_impl(core.as_ref(), panel.connection).await.unwrap();
    close_session_impl(core.as_ref(), session.id).await.unwrap();
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_host_key_is_asked_once_then_persisted() {
    let server = started_server().await;
    // 不预置信任：这台主机对 vault 来说是全新的
    let (core, mut rx) = live_core("tofu");
    let host_id = register_host(&core, &server);

    // 拨号会卡在「等用户点按钮」上，所以它必须与「回答」同时在跑
    let dial = {
        let core = Arc::clone(&core);
        tokio::spawn(async move { open_remote_impl(core.as_ref(), host_id, PtySize::default()).await })
    };
    let pending = match await_event(&core, &mut rx, "主机密钥询问", |e| matches!(e, BusEvent::HostKeyPending(_))).await {
        BusEvent::HostKeyPending(p) => p,
        other => panic!("期望主机密钥询问，收到 {other:?}"),
    };
    assert_eq!(pending.presented.base64_key, HOST_KEY_B64, "对话框里要显示服务器此刻出示的密钥");
    assert_ne!(pending.request, 0, "没有请求号前端就答不回来");
    // await_event 的代答已经被谓词拦下（谓词命中就不该代答），这里由用例自己答一次
    reply_impl(core.as_ref(), pending.request, UserReply::Confirmed).unwrap();

    let session = dial.await.expect("拨号任务不该被取消").expect("确认之后应当连上");

    // TOFU 的「一次」必须是真的：确认过就落盘，重新打开同一份 known_hosts 不再问
    let in_memory = core.known.lock().unwrap_or_else(|p| p.into_inner()).entries().to_vec();
    assert_eq!(in_memory.len(), 1, "确认后要留下一条信任记录");
    assert_eq!(in_memory[0].source, KnownHostSource::UserConfirmed);
    let reloaded = dbhx_vault::KnownHosts::load(core.dir()).expect("known_hosts 要真的落盘");
    // 落盘格式是 OpenSSH 的 `pattern algo key`，`added_at_ms`/`source` 不写回文件，
    // 所以只能比对这三样真正决定信任关系的东西
    assert_eq!(reloaded.entries().len(), 1, "重启后仍该只有一条记录");
    assert_eq!(reloaded.entries()[0].pattern, in_memory[0].pattern);
    assert_eq!(reloaded.entries()[0].base64_key, HOST_KEY_B64);
    match reloaded.validate(&in_memory[0]) {
        dbhx_model::HostKeyDecision::Trusted => {}
        other => panic!("已确认过的主机再次校验必须是 Trusted，收到 {other:?}"),
    }

    close_session_impl(core.as_ref(), session.id).await.unwrap();
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_host_key_blocks_the_connection_and_asks_nobody() {
    let server = started_server().await;
    let (core, mut rx) = live_core("changed");
    let host_id = register_host(&core, &server);

    // 先写一条**不一样**的密钥：真服务器的密钥于是变成「已变更」
    let stored = KnownHost {
        pattern: pattern_of(&server.host(), server.port()),
        algorithm: HOST_KEY_ALGO.to_string(),
        base64_key: "AAAAC3NzaC1lZDI1NTE5AAAAIInsteadOfTheRealOneAAAAAAAAAAAAAAAAAAA".to_string(),
        added_at_ms: 0,
        source: KnownHostSource::Imported,
    };
    core.known.lock().unwrap_or_else(|p| p.into_inner()).trust(stored.clone());

    let err = open_remote_impl(core.as_ref(), host_id, PtySize::default()).await.expect_err("密钥变更必须连不上");
    // actor 分不清「没记录」与「不一致」，所以错误码统一是 Unknown；
    // 真正告诉用户为什么连不上的是下面那条冲突事件
    assert_eq!(err.code, ErrorCode::HostKeyUnknown, "{:?}", err.code);
    assert!(!err.retryable, "中间人告警不该给出「重试」");

    let conflict = match await_event(&core, &mut rx, "主机密钥冲突", |e| matches!(e, BusEvent::HostKeyConflict(_))).await {
        BusEvent::HostKeyConflict(c) => c,
        other => panic!("期望冲突通知，收到 {other:?}"),
    };
    assert_eq!(conflict.stored.base64_key, stored.base64_key);
    assert_eq!(conflict.presented.base64_key, HOST_KEY_B64, "要让用户看见服务器现在出示的是哪把");
    assert_eq!(core.awaits.pending(), 0, "变更不给「仍然连接」，所以不该登记任何等待");
    assert_eq!(core.known.lock().unwrap_or_else(|p| p.into_inner()).entries().len(), 1, "被拒绝的密钥不能写盘");

    server.shutdown().await;
}

/// 按名字的操作要落在**用户正看着的那个目录**，不是 SFTP 会话的初始目录。
///
/// 真机上 root 用户的会话初始目录是 `/root`，所以「在 /data/logs 里把 a.txt 改名成
/// b.txt」如果直接把裸名字发给服务器，结果是把文件搬到 `/root/b.txt`——用户在 A 目录
/// 操作，东西出现在 B。这是双栏文件窗口实测踩到的，`browse` 早就按 `shown` 拼了，
/// `mkdir`/`rename` 是漏网的那两个。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bare_names_resolve_against_the_browsed_dir() {
    let server = started_server().await;
    let (core, mut rx) = live_core("naming");
    trust_the_server(&core, &server);
    let host_id = register_host(&core, &server);

    let session = open_remote_impl(core.as_ref(), host_id, PtySize { rows: 30, cols: 110 }).await.expect("远端 shell 应当开起来");
    await_event(&core, &mut rx, "初始目录", cwd_reported("/home/app")).await;
    let panel = open_for_session_impl(core.as_ref(), session.id).await.expect("面板应当免登录打开");

    // 手动进到子目录，之后面板显示的就是 /home/app/logs
    let entered = browse_impl(core.as_ref(), panel.connection, Some("logs".into())).await.unwrap();
    assert_eq!(entered.path, "/home/app/logs");

    mkdir_impl(core.as_ref(), panel.connection, "inbox".into(), false).await.unwrap();
    rename_impl(core.as_ref(), panel.connection, "/home/app/logs/app.log".into(), "app-2.log".into(), false).await.unwrap();

    let names = |listing: dbhx_model::DirListing| listing.entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
    let here = names(browse_impl(core.as_ref(), panel.connection, None).await.unwrap());
    assert!(here.contains(&"inbox".to_string()), "新建的目录要出现在正看着的这一栏，实际 {here:?}");
    assert!(here.contains(&"app-2.log".to_string()), "改名要原地改，实际 {here:?}");
    assert!(!here.contains(&"app.log".to_string()), "旧名字该没了，实际 {here:?}");
    assert_eq!(
        browse_impl(core.as_ref(), panel.connection, Some("/home/app/logs/inbox".into())).await.unwrap().entries.len(),
        0,
        "新建的得是个空目录"
    );

    // 这一条才是本次回归的核心：上层目录绝不能凭空多出这两个名字
    let parent = names(browse_impl(core.as_ref(), panel.connection, Some("/home/app".into())).await.unwrap());
    for leaked in ["inbox", "app-2.log"] {
        assert!(!parent.contains(&leaked.to_string()), "「{leaked}」被建到了上一级目录，说明裸名字没按显示目录解析：{parent:?}");
    }
    assert!(parent.contains(&"logs".to_string()), "上一级本来该有的东西不能被弄丢：{parent:?}");

    server.shutdown().await;
}

/// 新建空文件与改权限都作用在**面板正显示的目录**上，且属性真的写进了服务端。
///
/// 建文件最容易写错的一处是「拿 create 的截断语义去建同名文件」——用户敲错一次名字
/// 就把别人的文件清成 0 字节。夹具里 644→600 的回读则证明 chmod 走通了 SETATTRS，
/// 而不是客户端自己以为改了。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_file_and_chmod_act_on_the_browsed_dir() {
    let server = started_server().await;
    let (core, _rx) = live_core("editing");
    trust_the_server(&core, &server);
    let host_id = register_host(&core, &server);

    let session = open_remote_impl(core.as_ref(), host_id, PtySize { rows: 30, cols: 110 }).await.expect("远端 shell 应当开起来");
    let panel = open_for_session_impl(core.as_ref(), session.id).await.expect("面板应当免登录打开");
    browse_impl(core.as_ref(), panel.connection, Some("logs".into())).await.unwrap();

    create_file_impl(core.as_ref(), panel.connection, "note.txt".into()).await.expect("空文件应当建起来");

    let here = browse_impl(core.as_ref(), panel.connection, None).await.unwrap();
    let note = here.entries.iter().find(|e| e.name == "note.txt").expect("新建的文件要出现在正看着的这一栏");
    assert_eq!((note.kind, note.size, note.mode), (FileKind::File, 0, Some(0o644)), "新文件该是 0 字节的普通文件");

    // 同名再建必须是「已存在」，绝不能走截断
    let again = create_file_impl(core.as_ref(), panel.connection, "note.txt".into()).await;
    assert_eq!(again.err().map(|e| e.code), Some(ErrorCode::AlreadyExists), "同名文件被清空了");

    // 上层不该凭空多出这个名字——又一次证明裸名字必须按显示目录拼
    let upper = browse_impl(core.as_ref(), panel.connection, Some("/home/app".into())).await.unwrap();
    assert!(!upper.entries.iter().any(|e| e.name == "note.txt"), "文件被建到了会话初始目录：{upper:?}");

    chmod_impl(core.as_ref(), panel.connection, note.path.clone(), 0o600).await.expect("改权限应当落进服务端");
    // 上面那次检查把面板带到了上层目录，这里必须显式跳回来，否则读的是另一个目录
    let after = browse_impl(core.as_ref(), panel.connection, Some(here.path.clone())).await.unwrap();
    let changed = after.entries.iter().find(|e| e.name == "note.txt").expect("改完文件还在原处");
    assert_eq!(changed.mode, Some(0o600), "SETATTRS 没生效的话这里仍然是 644");

    // 越界的权限位是非法输入，不能被悄悄取模之后回一个成功
    let bad = chmod_impl(core.as_ref(), panel.connection, note.path.clone(), 0o10000).await;
    assert_eq!(bad.err().map(|e| e.code), Some(ErrorCode::InvalidInput), "越界权限位必须挡下来");

    server.shutdown().await;
}

/// 会话日志抄的是经过 hub 的那一份**原始字节**。
///
/// 只有对着真服务器才验得出意义：日志挂在唯一读者顺路抄，抄错会话、少抄一段、
/// 停止之后还在写，这些都是离线单测碰不到的接缝。抄的必须是**原始字节**（这里用
/// 终端的 CRLF 行尾做见证）——把转义序列洗掉之后，`top`、`vim` 那类全屏程序的日志
/// 就是一堆叠在一起的残骸，比留着更难读。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_log_copies_what_the_terminal_received_and_stops_when_told() {
    let server = started_server().await;
    let (core, mut rx) = live_core("logging");
    trust_the_server(&core, &server);
    let host_id = register_host(&core, &server);

    let session = open_remote_impl(core.as_ref(), host_id, PtySize { rows: 30, cols: 110 })
        .await
        .expect("远端 shell 应当开起来");
    await_event(&core, &mut rx, "初始目录", cwd_reported("/home/app")).await;

    let started = start_log_impl(core.as_ref(), session.id, None, "现场日志.log").await.expect("日志应当开起来");
    write_impl(core.as_ref(), session.id, b"echo dbhx-log-marker\r".to_vec()).await.unwrap();

    let text = wait_for_file_text(&started.path, "echo dbhx-log-marker").await;
    assert!(text.contains("dbhx-log-marker"), "命令与其回显都要落进日志：{text}");
    assert!(text.contains("\r\n"), "抄的是原始字节流，行尾不该被改成 \\n：{text:?}");

    let stopped = stop_log_impl(core.as_ref(), session.id).await.unwrap();
    assert_eq!(stopped.path, started.path, "停止回的路径必须就是开始那一个");
    assert!(stopped.bytes > 0, "已经抄到的字节数要如实报出来");
    let before = std::fs::read(&started.path).unwrap();

    write_impl(core.as_ref(), session.id, b"echo after-the-stop\r".to_vec()).await.unwrap();
    // 先把这条输出真正等到 hub 那儿走一趟，再回头看文件——否则「没写」只是因为还没到
    await_event(&core, &mut rx, "停止之后的输出", |e| match e {
        BusEvent::TermData(d) => d.session == session.id && String::from_utf8_lossy(&d.data.to_bytes().unwrap_or_default()).contains("after-the-stop"),
        _ => false,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read(&started.path).unwrap(), before, "停止之后一个字都不该再多写");

    close_session_impl(core.as_ref(), session.id).await.unwrap();
    server.shutdown().await;
}

/// 轮询等文件里出现某段文本。日志是 hub 顺路写的，没有事件可等。
async fn wait_for_file_text(path: &str, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(tokio::time::Instant::now() < deadline, "等「{needle}」落进 {path} 超时");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 登录横幅必须原样顺着 `term_data` 送到渲染层。
///
/// 这一段（`Welcome to Ubuntu …` + `Last login: …`）服务端只在登录那一瞬间打一次，
/// 之后**永远不会重画**。所以只要它在渲染层还没把终端建起来之前就到达，用户看到的
/// 就是一个凭空少了一屏的窗口——前端为此把「挂入格子之前」的输出先攒着，而攒的前提
/// 是后端确实把它发出来了。这条测试守住的是那个前提。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_login_banner_arrives_as_terminal_output() {
    let server = started_server().await;
    let (core, mut rx) = live_core("banner");
    trust_the_server(&core, &server);
    let host_id = register_host(&core, &server);

    let session = open_remote_impl(core.as_ref(), host_id, PtySize { rows: 30, cols: 110 })
        .await
        .expect("远端 shell 应当开起来");

    let contains_banner = |event: &BusEvent| match event {
        BusEvent::TermData(data) => banner_text(data).contains("Welcome to Ubuntu"),
        _ => false,
    };
    let arrived = await_event(&core, &mut rx, "登录横幅", contains_banner).await;
    let BusEvent::TermData(data) = arrived else {
        panic!("等待横幅时收到了别的事件：{arrived:?}")
    };
    assert_eq!(data.session, session.id, "横幅要落在刚开的那个会话上");
    let text = banner_text(&data);
    assert!(text.contains("Last login: "), "横幅该连同上次登录时间一起到：{text}");

    server.shutdown().await;
}

/// 事件里的 base64 解回文本。解不开就按空串算，让断言去报「没收到」。
fn banner_text(data: &TermData) -> String {
    let bytes = data.data.to_bytes().unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 横幅抢在 `shell` 的 `Success` 之前到达时，也必须一个字节都不少。
///
/// 真 sshd 里「回确认」和「fork 子进程往 pty 写横幅」是两条并行的路，谁先到纯看调度。
/// 客户端等 `Success` 的循环如果把先到的 `Data` 记条日志就扔掉，症状是连上主机后
/// 横幅时有时无，而且这段输出服务器只发一次，丢了永远补不回来。
/// 上面那条测试走的是「确认先到」的顺利时序，抓不住这个 bug。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_banner_arriving_before_the_success_is_not_swallowed() {
    let server = server_running(TestSpec::new().with_fs(sample_fs()).with_banner_before_success()).await;
    let (core, mut rx) = live_core("banner-race");
    trust_the_server(&core, &server);
    let host_id = register_host(&core, &server);

    let session = open_remote_impl(core.as_ref(), host_id, PtySize { rows: 30, cols: 110 })
        .await
        .expect("远端 shell 应当开起来");

    // 攒到提示符出现，再整段比对：少一块、错一位都过不了
    let mut got: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("等提示符超时")
            .expect("事件总线已关闭");
        match event {
            BusEvent::TermData(data) if data.session == session.id => {
                got.extend_from_slice(&data.data.to_bytes().unwrap_or_default());
                if String::from_utf8_lossy(&got).contains("dbhx-test:") {
                    break;
                }
            }
            // OSC 7 的目录事件与结束事件都可能插进来，本用例只清点字节流
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&got).into_owned();
    assert!(text.starts_with("Welcome to Ubuntu"), "横幅必须完整地排在最前面：{text}");
    assert!(text.contains("Last login: "), "横幅该连同上次登录时间一起到：{text}");
}

/// 连上「未知系统」的主机后，探测要把远端真实系统落库，并推事件让图标自己换。
///
/// 两条接缝一起钉住：探测命令真的经由 exec 通道到了服务端；整段过程只发生一次
/// 认证——探测借的是会话那条 transport，不是重新拨号。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_an_unknown_host_learns_the_remote_os() {
    let server = started_server().await;
    let (core, mut rx) = live_core("os-probe");
    trust_the_server(&core, &server);
    // 与 `draft()` 只差一个字段：新建时没选系统，记录停在「未知」
    let host_id = register_with(&core, HostDraft { os: OsType::Unknown, ..draft(&server) });

    // 登记阶段自己发过变更事件；先清干净，下面等的必须是探测发的那一条
    while rx.try_recv().is_ok() {}

    open_remote_impl(core.as_ref(), host_id.clone(), PtySize::default())
        .await
        .expect("远端 shell 应当开起来");
    await_event(&core, &mut rx, "探测落库", |e| matches!(e, BusEvent::VaultChanged(_))).await;

    assert_eq!(server.observed().auth_attempts.len(), 1, "探测必须借会话的 transport，不能再登录一次");
    assert!(
        server.observed().execs.iter().any(|c| c == dbhx_ipc::osdetect::PROBE_COMMAND),
        "服务端应当收到过探测命令：{:?}",
        server.observed().execs
    );
    let os = core.vault.lock().unwrap_or_else(|p| p.into_inner()).host(&host_id).map(|r| r.os);
    assert_eq!(os, Some(OsType::Ubuntu), "记录该照着远端报的系统更新");

    server.shutdown().await;
}
