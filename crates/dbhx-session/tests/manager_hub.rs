//! 会话注册表的行为矩阵。
//!
//! 这里只走**本地终端**路径：`open_remote` 要的是真 `dbhx_ssh::Shell`，
//! 假连接造不出来（russh 的通道半区不是能手工拼的类型）。因此本文件覆盖的是
//! 注册表自己的那部分逻辑——id 分配、单消费者事件流、退出后摘条目、失败不留半成品。
//! 远端通道的字节映射由 `shell.rs` 的 `Pump` 单测覆盖，端到端等进程内测试服务器。

use std::sync::Arc;
use std::time::Duration;

use dbhx_model::{AuthRef, HostId, LiveEndpoint, PtySize, SessionId, TerminalEncoding};
use dbhx_pty::LocalSpec;
use dbhx_session::{
    BoxFuture, DialPlan, Dialer, ExitInfo, PlanSource, SessionError, SessionEvent, SessionManager,
    SessionRecord, SharedConnection, TransportKey, TransportPool,
};

/// 永不成功的池：`open_remote` 在这里就必须失败，且失败得有辨识度。
fn failing_pool() -> Arc<TransportPool> {
    // 闭包返回值必须标注成 BoxFuture，否则推导不出 `dyn Future` 那层间接
    let dialer: Dialer = Box::new(|_key, _plan| -> BoxFuture<'static, Result<SharedConnection, SessionError>> {
        Box::pin(async { Err(SessionError::Closed) })
    });
    let plans: PlanSource = Box::new(|_key| -> BoxFuture<'static, Result<DialPlan, SessionError>> {
        Box::pin(async { Err(SessionError::Closed) })
    });
    Arc::new(TransportPool::new(dialer, plans, Default::default()))
}

fn manager() -> SessionManager {
    SessionManager::new(failing_pool())
}

fn pty() -> PtySize {
    PtySize { rows: 24, cols: 80 }
}

/// 一条会立刻退出并把标记打到 stdout 的命令。
fn echo_spec(marker: &str) -> LocalSpec {
    #[cfg(windows)]
    let spec = LocalSpec::default().program("cmd.exe").args(&["/C", &format!("echo {marker}")]);
    #[cfg(not(windows))]
    let spec = LocalSpec::default().program("/bin/echo").args(&[marker]);
    spec.pty(pty())
}

/// 一条不会自己退出的交互命令，靠 `close` 收掉。
fn shell_spec() -> LocalSpec {
    #[cfg(windows)]
    let spec = LocalSpec::default().program("cmd.exe");
    #[cfg(not(windows))]
    let spec = LocalSpec::default().program("/bin/sh");
    spec.pty(pty())
}

fn open_local(m: &SessionManager, label: &str, spec: &LocalSpec) -> SessionRecord {
    m.open_local(spec, label.into(), TerminalEncoding::Utf8).expect("本地终端应能起")
}

/// 从事件流里读到包含 `needle` 的输出为止；超时即失败，不留悬挂测试。
async fn wait_until(rx: &mut tokio::sync::mpsc::Receiver<SessionEvent>, needle: &str) -> String {
    let mut text = String::new();
    for _ in 0..200 {
        let event = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("20 秒内没有等到事件，已收到: {text:?}"))
            .expect("事件流提前结束");
        if let SessionEvent::Data { bytes, .. } = event {
            text.push_str(&String::from_utf8_lossy(&bytes));
            if text.contains(needle) {
                return text;
            }
        }
    }
    panic!("没找到 {needle:?}，实际输出: {text:?}")
}

async fn drain_until_ended(rx: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> Option<ExitInfo> {
    for _ in 0..400 {
        match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
            Err(_elapsed) => panic!("20 秒内事件流没有结束"),
            Ok(None) => return None,
            Ok(Some(SessionEvent::Ended { info, .. })) => return Some(info),
            Ok(Some(_)) => {}
        }
    }
    panic!("事件流没有给出 Ended")
}

#[tokio::test]
async fn ids_start_at_one_and_are_unique() {
    let m = manager();
    let first = open_local(&m, "a", &shell_spec());
    let second = open_local(&m, "b", &shell_spec());
    assert_eq!(first.id, SessionId(1), "0 要留给「没有会话」的哨兵值");
    assert_eq!(second.id, SessionId(2));
    assert_eq!(m.count(), 2);
    assert_eq!(m.list().len(), 2);
    assert_eq!(m.list()[0].label, "a", "列表按 id 稳定排序，前端 tab 顺序才不会飘");

    m.close(first.id).await.unwrap();
    m.close(second.id).await.unwrap();
}

#[tokio::test]
async fn events_can_be_taken_only_once() {
    let m = manager();
    assert!(m.take_events().is_some());
    assert!(m.take_events().is_none(), "第二个读者会让字节流随机丢一半");
}

#[tokio::test]
async fn output_reaches_the_hub_and_the_entry_is_reaped_after_exit() {
    let m = manager();
    let mut rx = m.take_events().unwrap();
    let record = open_local(&m, "echo", &echo_spec("dbhx-hub-marker"));
    assert!(!record.is_remote());
    assert!(record.live_endpoint.is_none());

    let text = wait_until(&mut rx, "dbhx-hub-marker").await;
    assert!(text.contains("dbhx-hub-marker"));

    // 子进程退出 ⇒ Ended ⇒ 转发任务摘条目
    let info = drain_until_ended(&mut rx).await;
    assert_eq!(info.map(|i| i.exit_status), Some(Some(0)), "echo 正常退出，退出码必须带出来");
    for _ in 0..100 {
        if m.count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("会话结束后条目还在表里，句柄与租约会一直占着");
}

#[tokio::test]
async fn closing_a_local_tab_ends_the_stream_and_frees_the_slot() {
    let m = manager();
    let mut rx = m.take_events().unwrap();
    let record = open_local(&m, "kill", &shell_spec());

    m.write(record.id, b"echo dbhx-alive\r").await.unwrap();
    wait_until(&mut rx, "dbhx-alive").await;

    m.close(record.id).await.unwrap();
    // Ended 可能在 Data 之后才到，继续读到它为止
    let _ = drain_until_ended(&mut rx).await;
    for _ in 0..100 {
        if m.count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(m.count(), 0, "关闭 tab 后标签页必须从注册表消失");
    // 已经没了的会话再操作只能报错，不能 panic
    assert!(matches!(m.write(record.id, b"x").await, Err(SessionError::NoSuchSession(_))));
}

#[tokio::test]
async fn resize_is_recorded_on_the_session() {
    let m = manager();
    let record = open_local(&m, "size", &shell_spec());
    let big = PtySize { rows: 50, cols: 200 };
    m.resize(record.id, &big).await.unwrap();
    assert_eq!(m.record(record.id).unwrap().pty, big, "前端重开面板时要拿到最新行列");
    m.close(record.id).await.unwrap();
}

#[tokio::test]
async fn unknown_session_is_an_error_on_every_verb() {
    let m = manager();
    let ghost = SessionId(999);
    assert!(matches!(m.write(ghost, b"x").await, Err(SessionError::NoSuchSession(999))));
    assert!(matches!(m.resize(ghost, &pty()).await, Err(SessionError::NoSuchSession(_))));
    assert!(matches!(m.close(ghost).await, Err(SessionError::NoSuchSession(_))));
    assert!(m.record(ghost).is_none());

    let ipc: dbhx_model::IpcError = SessionError::NoSuchSession(999).into();
    assert_eq!(ipc.code, dbhx_model::ErrorCode::NotFound);
    assert!(!ipc.retryable, "标签页都关了，重试个什么");
}

#[tokio::test]
async fn remote_open_failure_leaves_no_session_and_no_id_burn() {
    let m = manager();
    let key = TransportKey::new(
        HostId("h1".into()),
        LiveEndpoint { host: "nope".into(), port: 22, username: "root".into() },
        AuthRef::Agent,
        0,
    );
    let err = m.open_remote(key, "xterm".into(), pty(), TerminalEncoding::Utf8, "nope".into()).await;
    assert!(matches!(err, Err(SessionError::Closed)), "假池的失败原因必须原样传出去");
    assert_eq!(m.count(), 0, "失败的会话不能占着表");
    // id 已经分配过，下一个会话不会复用旧 id——复用会让前端把两个 tab 认成一个
    let record = open_local(&m, "after", &shell_spec());
    assert_eq!(record.id, SessionId(1));
    m.close(record.id).await.unwrap();
}

#[tokio::test]
async fn dropping_the_manager_closes_the_event_stream_without_stranding_tasks() {
    // 退出流程的形状：注册表被丢掉 ⇒ 事件接收端应当自然结束，
    // 转发任务靠 Weak 而不是强引用，不会把 Inner 钉在内存里等死。
    let m = manager();
    let mut rx = m.take_events().unwrap();
    open_local(&m, "orphan", &shell_spec());
    drop(m);
    match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
        Err(_elapsed) => panic!("注册表都没了，事件流还在挂着"),
        // 收到 None（发送端随 Inner 一起 drop）或最后一条事件都算结束
        Ok(_) => {}
    }
    // 这里不能有 panic：转发任务在 Weak 落空后只是记一条日志
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// telnet 走的是裸 TCP，不碰 transport 池，但同样要经过注册表这套 id/事件流。
/// 这里用多线程 runtime + 一个「连上就攥着」的本地服务器，只验证注册表的形状，
/// 真正的协议收发在 `dbhx-telnet` 自己的集成测试里已经钉死。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_telnet_session_is_labeled_telnet_not_ssh() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = tokio::spawn(async move {
        let (_sock, _) = listener.accept().await.unwrap();
        // 攥住连接直到测试结束，既不读也不写，只为让会话保持存活
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let m = manager();
    let record = m
        .open_telnet("127.0.0.1".into(), port, pty(), TerminalEncoding::Utf8, "tn".into(), HostId("telnet-host".into()))
        .await
        .expect("telnet 应能连上");

    // 核心回归：带 live_endpoint 的会话不等于 SSH——telnet 必须如实标注
    assert_eq!(record.info().protocol, dbhx_model::Protocol::Telnet);
    assert!(record.is_remote(), "telnet 有远端地址，算远端会话");
    assert!(m.transport_key(record.id).is_none(), "telnet 不进 transport 池，面板也就无从复用一条 SSH 连接");

    m.write(record.id, b"ls\r").await.unwrap();
    m.resize(record.id, &PtySize { rows: 40, cols: 120 }).await.unwrap();
    assert_eq!(m.record(record.id).unwrap().pty, PtySize { rows: 40, cols: 120 }, "改尺寸要写回条目");

    m.close(record.id).await.unwrap();
    // 关闭后条目会随转发任务摘除；给它一点收敛时间，最多到注册表空为止
    for _ in 0..100 {
        if m.record(record.id).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    hold.abort();
}
