//! 本机监听侧的端口转发。
//!
//! 这一层只认识「把一条 TCP 字节流交给 opener，再双向搬字节」，不知道 SSH、vault 或
//! Tauri 的存在。opener 由会话层注入（生产实现是 `direct-tcpip` 通道，测试实现直接
//! 连本地回显服务器），因此监听、SOCKS5 解析与管道逻辑都能离线测试。
//!
//! 两种形态：
//! - **本地转发**：所有连到 `bind` 的流量都去固定的远端目标；
//! - **动态转发**：连上来先跑 SOCKS5，目标由客户端在握手时指定。
//!
//! 远端监听（`tcpip-forward`）不在这里：那需要服务器主动开通道，属于 `dbhx-ssh` 的
//! handler 职责。

mod socks5;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// 一条已准备好的双向字节流：SSH 通道、本地 TCP 都行。
pub trait TunnelIo: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> TunnelIo for T {}

pub type TunnelStream = Box<dyn TunnelIo>;

/// 打开一条通往目标的服务端通道。实现方负责把目标地址交给 SSH。
pub trait ChannelOpener: Send + Sync + 'static {
    fn open(
        &self,
        host: String,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<TunnelStream, TunnelError>> + Send + 'static>>;
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("绑定 {0} 失败: {1}")]
    Bind(String, #[source] std::io::Error),
    #[error("SOCKS5 握手失败: {0}")]
    Socks(String),
    #[error("打开远端通道失败: {0}")]
    Open(String),
    #[error("转发 I/O 失败: {0}")]
    Io(#[source] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelTarget {
    /// 本地转发：所有连接都去这个目标
    Fixed { host: String, port: u16 },
    /// 动态转发：目标由 SOCKS5 客户端指定
    Socks5,
}

/// 一个正在监听的本地转发。drop 即停止。
pub struct LocalTunnel {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl LocalTunnel {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for LocalTunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 绑定并开始监听。`bind_port` 传 0 时由系统分配，实际地址见 [`LocalTunnel::addr`]。
pub async fn spawn_local(
    opener: Arc<dyn ChannelOpener>,
    bind_host: &str,
    bind_port: u16,
    target: TunnelTarget,
) -> Result<LocalTunnel, TunnelError> {
    let label = format!("{bind_host}:{bind_port}");
    let listener = TcpListener::bind((bind_host, bind_port))
        .await
        .map_err(|e| TunnelError::Bind(label.clone(), e))?;
    let addr = listener
        .local_addr()
        .map_err(|e| TunnelError::Bind(label, e))?;
    tracing::info!(%addr, ?target, "本地转发开始监听");

    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((conn, peer)) => {
                    let opener = Arc::clone(&opener);
                    let target = target.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_one(opener, target, conn).await {
                            tracing::debug!(%peer, %e, "转发连接结束");
                        }
                    });
                }
                Err(e) => {
                    // accept 失败通常是被中断或资源暂缺，监听器继续跑
                    tracing::warn!(%e, "转发监听 accept 失败");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    Ok(LocalTunnel { addr, task })
}

async fn serve_one(
    opener: Arc<dyn ChannelOpener>,
    target: TunnelTarget,
    mut conn: TcpStream,
) -> Result<(), TunnelError> {
    let (host, port) = match target {
        TunnelTarget::Fixed { host, port } => (host, port),
        TunnelTarget::Socks5 => socks5::handshake(&mut conn).await?,
    };
    let mut upstream = opener.open(host, port).await?;
    tokio::io::copy_bidirectional(&mut conn, &mut upstream)
        .await
        .map_err(TunnelError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct DirectOpener {
        target: SocketAddr,
    }

    impl ChannelOpener for DirectOpener {
        fn open(
            &self,
            _host: String,
            _port: u16,
        ) -> Pin<Box<dyn Future<Output = Result<TunnelStream, TunnelError>> + Send + 'static>>
        {
            let target = self.target;
            Box::pin(async move {
                let stream = TcpStream::connect(target)
                    .await
                    .map_err(|e| TunnelError::Open(e.to_string()))?;
                Ok(Box::new(stream) as TunnelStream)
            })
        }
    }

    /// 只跑一次回显连接的后台服务。
    async fn echo_once() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut conn, _)) = listener.accept().await {
                let mut buf = [0u8; 64];
                while let Ok(n) = conn.read(&mut buf).await {
                    if n == 0 || conn.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn local_tunnel_pipes_bytes_through_the_opener() {
        let echo = echo_once().await;
        let opener = Arc::new(DirectOpener { target: echo });
        let tunnel = spawn_local(
            opener,
            "127.0.0.1",
            0,
            TunnelTarget::Fixed {
                host: "ignored".into(),
                port: 0,
            },
        )
        .await
        .unwrap();

        let mut client = TcpStream::connect(tunnel.addr()).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut back))
            .await
            .expect("等回显超时")
            .unwrap();
        assert_eq!(&back, b"hello");
    }

    #[tokio::test]
    async fn dynamic_tunnel_speaks_socks5_then_pipes() {
        let echo = echo_once().await;
        let opener = Arc::new(DirectOpener { target: echo });
        let tunnel = spawn_local(opener, "127.0.0.1", 0, TunnelTarget::Socks5)
            .await
            .unwrap();

        let mut client = TcpStream::connect(tunnel.addr()).await.unwrap();
        // 问候：版本 5，只提供「无认证」
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0u8; 2];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);

        // CONNECT example.com:80
        let mut request = vec![5, 1, 0, 3, 11];
        request.extend_from_slice(b"example.com");
        request.extend_from_slice(&80u16.to_be_bytes());
        client.write_all(&request).await.unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0, "SOCKS5 应当报告成功");

        client.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut back))
            .await
            .expect("等回显超时")
            .unwrap();
        assert_eq!(&back, b"ping");
    }
}
