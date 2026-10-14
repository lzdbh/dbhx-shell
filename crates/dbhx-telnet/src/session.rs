//! telnet 会话：一条 TCP 连接 + IAC 状态机 + 原始字节双向透传。
//!
//! 形状刻意贴着 [`dbhx_pty`] 的本地会话：**读写与协商合并在一个任务里**，用
//! `select!` 同时盯着 socket 与出站队列。之所以不是一个读任务一个写任务，是因为
//! 出站队列里既有「用户敲的字节」也有「对协商的回应」，两者都往同一个 write half 写；
//! 分成两个任务就得再为 write half 加一层锁或第二条通道，而 telnet 的吞吐根本不配。
//!
//! 没有池、没有 lease、没有主机密钥：telnet 明文跑在 TCP 上，这些 SSH 专属的东西
//! 一个都不适用。登录与口令是**在流里**发生的事（服务器发 `login:` 提示、用户直接敲），
//! 所以这一层连「凭据」这个概念都没有，也就天然满足了「明文不跨 IPC」。

use std::sync::Arc;

use dbhx_model::PtySize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::TelnetError;
use crate::iac::{self, IacParser};

/// 出站帧：用户数据（已转义）、窗口变化、或关闭。
enum Frame {
    /// 用户敲进去、已经过 `escape` 的字节。
    Data(Vec<u8>),
    /// 尺寸变了，重发一次 NAWS。
    Naws { cols: u16, rows: u16 },
    /// 主动关闭。
    Close,
}

/// 会话往外冒的事件：终端字节，或结束。
#[derive(Debug, Clone)]
pub enum TelnetEvent {
    Data(Arc<[u8]>),
    Closed,
}

/// 控制一个 telnet 会话的句柄。可克隆，克隆共享同一条出站队列。
#[derive(Clone)]
pub struct TelnetHandle {
    tx: tokio::sync::mpsc::Sender<Frame>,
}

impl TelnetHandle {
    /// 写用户输入。裸 `0xFF` 会在这里被转义成 `IAC IAC`，绝不让它逃出去变成命令。
    pub fn write(&self, bytes: &[u8]) -> Result<(), TelnetError> {
        self.tx.try_send(Frame::Data(iac::escape(bytes))).map_err(|_| TelnetError::Closed)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), TelnetError> {
        self.tx.try_send(Frame::Naws { cols, rows }).map_err(|_| TelnetError::Closed)
    }

    /// 关掉会话：让任务退出并 drop socket。
    pub fn close(&self) {
        // 发不出去说明任务已经没了，正合我意
        let _ = self.tx.try_send(Frame::Close);
    }
}

/// 起一个 telnet 会话：连上远端，跑起读/协商/写一体的任务。
///
/// 返回 `(句柄, 事件接收端)`。连接失败以 [`TelnetError::Io`] 回，交上层翻成
/// `unreachable`/`timeout`——telnet 没有认证失败之外的分类可言。
pub async fn connect(host: &str, port: u16, pty: &PtySize) -> Result<(TelnetHandle, tokio::sync::mpsc::Receiver<TelnetEvent>), TelnetError> {
    let stream = TcpStream::connect((host, port)).await?;
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(512);
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Frame>(64);

    let mut cols = pty.cols;
    let mut rows = pty.rows;

    tokio::spawn(async move {
        let (mut rd, mut wr) = stream.into_split();
        let mut parser = IacParser::new();
        let mut buf = [0u8; 8192];
        // 一连上先声明 WILL NAWS 并报到当前大小，多数服务器据此决定回显宽度
        let _ = wr.write_all(&iac::greeting(cols, rows)).await;

        loop {
            tokio::select! {
                read = rd.read(&mut buf) => match read {
                    Ok(0) => break,
                    Ok(n) => {
                        let parsed = parser.feed(&buf[..n]);
                        if !parsed.data.is_empty() && events_tx.send(TelnetEvent::Data(Arc::from(parsed.data))).await.is_err() {
                            break;
                        }
                        for cmd in parsed.commands {
                            let out = iac::reply_to(&cmd, cols, rows);
                            if !out.is_empty() && wr.write_all(&out).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                },
                frame = frame_rx.recv() => match frame {
                    None => break,
                    Some(Frame::Data(bytes)) => {
                        if wr.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Some(Frame::Naws { cols: c, rows: r }) => {
                        cols = c;
                        rows = r;
                        let _ = wr.write_all(&iac::naws(c, r)).await;
                    }
                    Some(Frame::Close) => break,
                },
            }
        }
        // 无论怎么退出的，都补一个结束事件；接收端已关就丢弃
        let _ = events_tx.send(TelnetEvent::Closed).await;
    });

    Ok((TelnetHandle { tx: frame_tx }, events_rx))
}
