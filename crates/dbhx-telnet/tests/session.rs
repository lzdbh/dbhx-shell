//! 进程内 telnet 服务器 ⇒ 离线跑通「握手 + 转义 + 关闭」这条唯一的验证路径。
//!
//! 之所以自建服务器而不是连公网：telnet 的对端行为（发 WILL ECHO、在数据里塞 0xFF、
//! 突然断开）都得由测试精确摆出，才能钉死客户端的两个不变量——协商字节绝不漏进终端、
//! 用户数据里的 0xFF 绝不逃出去变成命令。

use dbhx_telnet::{TelnetEvent, connect, iac};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 把监听端口告诉调用方：绑 0 让 OS 分配，测完随作用域结束自然释放。
async fn listen() -> (u16, TcpListener) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    (l.local_addr().unwrap().port(), l)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiates_escapes_and_closes_against_a_live_server() {
    let (port, listener) = listen().await;
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // 1. 先吃掉客户端的问候：WILL NAWS(3) + NAWS 子协商(9)
        let mut greeting = [0u8; 12];
        sock.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting[..3], &[iac::IAC, iac::WILL, iac::OPT_NAWS]);
        assert_eq!(&greeting[3..], &iac::naws(80, 24)[..]);

        // 2. 声明启用回声——客户端应当答 DO
        sock.write_all(&[iac::IAC, iac::WILL, iac::OPT_ECHO]).await.unwrap();
        let mut reply = [0u8; 3];
        sock.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [iac::IAC, iac::DO, iac::OPT_ECHO]);

        // 3. 推一段带 0xFF 的数据。要送出「数据里的一个 0xFF」，服务器必须像客户端
        //    转义输入那样把它写成 `IAC IAC`——裸 0xFF 是命令引导符，会被当成一条对话。
        sock.write_all(&iac::escape(b"pong\xFF!")).await.unwrap();

        // 4. 客户端应当把窗口变化重新报上来，再把用户输入转义后送出
        let mut naws = [0u8; 9];
        sock.read_exact(&mut naws).await.unwrap();
        assert_eq!(naws, iac::naws(132, 43)[..]);
        let mut input = [0u8; 4];
        sock.read_exact(&mut input).await.unwrap();
        assert_eq!(input, [b'a', iac::IAC, iac::IAC, b'b']);

        // 5. 断开——客户端读到 EOF 后应当收敛成 Closed 事件
        sock.shutdown().await.unwrap();
        drop(sock);
    });

    let (handle, mut events) = connect("127.0.0.1", port, &dbhx_model::PtySize { rows: 24, cols: 80 }).await.unwrap();

    // 握手先落地：服务器发 WILL ECHO，客户端只可能回 DO ECHO（此刻出站队列是空的，
    // select! 里没有别的帧可写，顺序才是确定的）。等数据到手再驱动出站。
    let mut collected: Vec<u8> = Vec::new();
    while collected.len() < 6 {
        match events.recv().await {
            Some(TelnetEvent::Data(bytes)) => collected.extend_from_slice(&bytes),
            other => panic!("期望终端数据，收到 {other:?}"),
        }
    }
    assert_eq!(collected, b"pong\xFF!", "转义的 0xFF 应还原成数据，协商字节绝不能漏进来");

    // 数据到手后再驱动出站：窗口变化 + 带 0xFF 的输入
    handle.resize(132, 43).unwrap();
    handle.write(b"a\xFFb").unwrap();

    assert!(matches!(events.recv().await, Some(TelnetEvent::Closed)));

    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writing_after_the_peer_is_gone_reports_closed() {
    let (port, listener) = listen().await;
    let server = tokio::spawn(async move {
        // 立刻接受并关闭，制造一个「连上就没」的对端
        let (sock, _) = listener.accept().await.unwrap();
        drop(sock);
    });

    let (handle, mut events) = connect("127.0.0.1", port, &dbhx_model::PtySize { rows: 24, cols: 80 }).await.unwrap();
    // 读到 None 意味着会话任务已经结束：那一刻出站帧通道也随任务一起 drop 了，
    // 只有到这个点再写，「写失败」才是确定的，而不是抢在任务退出之前的竞态。
    loop {
        match events.recv().await {
            None => break,
            Some(TelnetEvent::Data(_)) => {}
            Some(TelnetEvent::Closed) => {}
        }
    }

    server.await.unwrap();
    // 会话没了之后继续写，应当明确失败而不是静默丢弃
    assert!(matches!(handle.write(b"x"), Err(dbhx_telnet::TelnetError::Closed)));
}
