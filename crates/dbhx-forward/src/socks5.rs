//! 最小 SOCKS5 CONNECT 解析。只支持 RFC 1928 的无认证方式与 CONNECT 命令；
//! BIND/UDP ASSOCIATE 直接拒绝（桌面应用的动态转发用不到）。

use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::TunnelError;

const VERSION: u8 = 5;
const CMD_CONNECT: u8 = 1;
const ATYP_IPV4: u8 = 1;
const ATYP_DOMAIN: u8 = 3;
const ATYP_IPV6: u8 = 4;

pub async fn handshake<S>(stream: &mut S) -> Result<(String, u16), TunnelError>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 2];
    stream
        .read_exact(&mut head)
        .await
        .map_err(|e| TunnelError::Socks(e.to_string()))?;
    if head[0] != VERSION {
        return Err(TunnelError::Socks(format!("版本 {} 不是 5", head[0])));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    stream
        .read_exact(&mut methods)
        .await
        .map_err(|e| TunnelError::Socks(e.to_string()))?;
    if !methods.contains(&0) {
        let _ = stream.write_all(&[VERSION, 0xFF]).await;
        return Err(TunnelError::Socks("客户端不支持无认证方式".into()));
    }
    stream
        .write_all(&[VERSION, 0])
        .await
        .map_err(|e| TunnelError::Socks(e.to_string()))?;

    let mut request = [0u8; 4];
    stream
        .read_exact(&mut request)
        .await
        .map_err(|e| TunnelError::Socks(e.to_string()))?;
    if request[0] != VERSION || request[1] != CMD_CONNECT {
        reject(stream, 7).await;
        return Err(TunnelError::Socks("只支持 CONNECT".into()));
    }

    let host = match request[3] {
        ATYP_IPV4 => {
            let mut ip = [0u8; 4];
            stream
                .read_exact(&mut ip)
                .await
                .map_err(|e| TunnelError::Socks(e.to_string()))?;
            Ipv4Addr::from(ip).to_string()
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .await
                .map_err(|e| TunnelError::Socks(e.to_string()))?;
            let mut domain = vec![0u8; usize::from(len[0])];
            stream
                .read_exact(&mut domain)
                .await
                .map_err(|e| TunnelError::Socks(e.to_string()))?;
            String::from_utf8(domain).map_err(|_| TunnelError::Socks("域名不是 UTF-8".into()))?
        }
        ATYP_IPV6 => {
            let mut ip = [0u8; 16];
            stream
                .read_exact(&mut ip)
                .await
                .map_err(|e| TunnelError::Socks(e.to_string()))?;
            Ipv6Addr::from(ip).to_string()
        }
        other => {
            reject(stream, 8).await;
            return Err(TunnelError::Socks(format!("不支持的地址类型 {other}")));
        }
    };
    let mut port = [0u8; 2];
    stream
        .read_exact(&mut port)
        .await
        .map_err(|e| TunnelError::Socks(e.to_string()))?;

    // 成功应答里的 BND.ADDR 填 0.0.0.0:0：我们没有自己的监听地址要回报
    stream
        .write_all(&[VERSION, 0, 0, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await
        .map_err(|e| TunnelError::Socks(e.to_string()))?;
    Ok((host, u16::from_be_bytes(port)))
}

async fn reject<S: AsyncWrite + Unpin>(stream: &mut S, code: u8) {
    let _ = stream
        .write_all(&[VERSION, code, 0, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_ipv4_connect() {
        let (mut client, mut server) = tokio::io::duplex(256);
        tokio::spawn(async move {
            client.write_all(&[5, 1, 0]).await.unwrap();
            client
                .write_all(&[5, 1, 0, 1, 10, 0, 0, 7, 0x1F, 0x90])
                .await
                .unwrap();
            let mut ok = [0u8; 2];
            client.read_exact(&mut ok).await.unwrap();
            let mut reply = [0u8; 8];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[1], 0);
        });
        let (host, port) = handshake(&mut server).await.unwrap();
        assert_eq!(host, "10.0.0.7");
        assert_eq!(port, 8080);
    }

    #[tokio::test]
    async fn parses_domain_connect_and_keeps_the_name() {
        let (mut client, mut server) = tokio::io::duplex(256);
        tokio::spawn(async move {
            client.write_all(&[5, 1, 0]).await.unwrap();
            let mut req = vec![5, 1, 0, 3, 11];
            req.extend_from_slice(b"example.com");
            req.extend_from_slice(&443u16.to_be_bytes());
            client.write_all(&req).await.unwrap();
            let mut drain = [0u8; 12];
            client.read_exact(&mut drain).await.unwrap();
        });
        let (host, port) = handshake(&mut server).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[tokio::test]
    async fn rejects_a_non_connect_command() {
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            client.write_all(&[5, 1, 0]).await.unwrap();
            let mut drain = [0u8; 2];
            client.read_exact(&mut drain).await.unwrap();
            client
                .write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let mut reply = [0u8; 10];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[1], 7);
        });
        assert!(matches!(
            handshake(&mut server).await,
            Err(TunnelError::Socks(_))
        ));
    }
}
