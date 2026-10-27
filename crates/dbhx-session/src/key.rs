//! transport 身份键：判等即「这两个请求能不能共用同一条 SSH 连接」。

use std::time::Duration;

use dbhx_model::{AuthRef, HostId, LiveEndpoint};

/// 参考实现用 10 元组（含跳板链指纹、代理指纹、协议、sudo 等）。M1 只有 SSH + vault
/// 凭据，所以只保留真正会改变「连到哪、以谁的身份、按什么算法偏好」的四个量。
/// 将来加跳板机时在这里补 `jump_fingerprint`，不要另起一套键。
///
/// 刻意 `Hash` 里没有明文：`auth` 只是引用，凭据本身连这个结构体都进不来。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransportKey {
    /// 同一 endpoint 在 vault 里可能是两条记录（不同分组、不同凭据）。按参考实现的语义
    /// **不跨记录复用**：两套身份串到同一条连接上，出了问题无法向用户解释。
    pub host_id: HostId,
    /// **会话的实时 endpoint**，不是 vault 里的静态记录。用户连上之后改了主机名/端口/
    /// 用户名，旧连接不能算同一个目标——少了这一条，编辑主机就会每次误复用或误另开。
    pub endpoint: LiveEndpoint,
    pub auth: AuthRef,
    /// keepalive 会进 `russh::client::Config`，两套设置不该共用一条连接。
    pub keepalive: Option<Duration>,
}

impl TransportKey {
    pub fn new(host_id: HostId, endpoint: LiveEndpoint, auth: AuthRef, keepalive_secs: u16) -> Self {
        Self {
            host_id,
            endpoint,
            auth,
            keepalive: if keepalive_secs == 0 { None } else { Some(Duration::from_secs(keepalive_secs as u64)) },
        }
    }

    /// 日志与错误文案用。带用户名是有意的——运维要靠它认出是哪台机器；
    /// 这个串**不允许**跨 IPC 给前端（前端自己有 label）。
    pub fn describe(&self) -> String {
        format!(
            "{}@{}:{}#{}",
            self.endpoint.username, self.endpoint.host, self.endpoint.port, self.host_id.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::{Endpoint, HostId};

    fn key(host: &str, port: Option<u16>, user: &str, secs: u16) -> TransportKey {
        let endpoint = Endpoint { host: host.into(), port, username: user.into() };
        TransportKey::new(
            HostId("h1".into()),
            LiveEndpoint::from_endpoint(&endpoint),
            AuthRef::Host { host_id: "h1".into() },
            secs,
        )
    }

    #[test]
    fn same_target_same_key() {
        assert_eq!(key("db1", Some(22), "root", 30), key("db1", Some(22), "root", 30));
    }

    #[test]
    fn port_and_username_are_part_of_the_identity() {
        assert_ne!(key("db1", Some(22), "root", 30), key("db1", Some(2222), "root", 30));
        assert_ne!(key("db1", Some(22), "root", 30), key("db1", Some(22), "app", 30));
    }

    #[test]
    fn missing_port_normalizes_to_22_instead_of_becoming_its_own_identity() {
        // 判等必须建立在归一化后的 LiveEndpoint 上，否则 None 与 Some(22) 会各开一条连接
        assert_eq!(key("db1", None, "root", 30), key("db1", Some(22), "root", 30));
    }

    #[test]
    fn different_keepalive_is_a_different_transport_config() {
        assert_ne!(key("db1", Some(22), "root", 30), key("db1", Some(22), "root", 0));
    }

    #[test]
    fn zero_keepalive_is_none_not_a_one_second_timer() {
        assert_eq!(key("db1", None, "root", 0).keepalive, None);
    }

    #[test]
    fn keys_are_hashable_into_a_map() {
        let mut seen = std::collections::HashSet::new();
        seen.insert(key("db1", None, "root", 30));
        assert!(seen.contains(&key("db1", Some(22), "root", 30)));
    }
}
