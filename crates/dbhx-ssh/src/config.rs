//! 拨号参数与 `russh::client::Config` 的构造。
//!
//! 只在这里碰算法偏好。默认值刻意跟随 russh（它是按当下主流 OpenSSH 协商结果选的），
//! 我们的改动必须每一条都有理由，因为「手工列算法表」是 SSH 客户端最常见的自残行为。

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use dbhx_model::LiveEndpoint;
use russh::client::Config;
use russh::SshId;

#[derive(Debug, Clone)]
pub struct DialOptions {
    pub endpoint: LiveEndpoint,
    /// TCP 连接 + SSH id 交换 + 密钥交换的总体预算。
    /// 10 秒：跨机房跳板机冷启动见过 6 秒，再长用户就已经以为程序挂了。
    pub handshake_timeout: Duration,
    /// 空闲多久发一次 keepalive。来自 `HostRecord.keepalive_secs`，`None` 表示不发。
    pub keepalive: Option<Duration>,
}

impl DialOptions {
    pub fn new(endpoint: LiveEndpoint) -> Self {
        Self {
            endpoint,
            handshake_timeout: Duration::from_secs(10),
            keepalive: Some(Duration::from_secs(30)),
        }
    }

    pub fn with_keepalive(mut self, secs: u64) -> Self {
        self.keepalive = if secs == 0 { None } else { Some(Duration::from_secs(secs)) };
        self
    }

    pub(crate) fn config(&self) -> Arc<Config> {
        let mut cfg = Config::default();
        // 关掉 Nagle。终端里每个按键都是一个 1 字节包，攒着发会让远端肉眼可见地迟钝。
        cfg.nodelay = true;
        cfg.keepalive_interval = self.keepalive;
        // 不做 inactivity_timeout：连接该活多久由会话层的 lease/idle park 决定，
        // 而不是由 russh 偷偷把 transport 拆掉。
        cfg.inactivity_timeout = None;
        cfg.client_id = SshId::Standard(Cow::Owned(format!(
            "SSH-2.0-dbhx-shell_{}",
            env!("CARGO_PKG_VERSION")
        )));
        Arc::new(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::{Endpoint, PtySize};

    fn ep() -> LiveEndpoint {
        LiveEndpoint::from_endpoint(&Endpoint {
            host: "h".into(),
            port: Some(2222),
            username: "u".into(),
        })
    }

    #[test]
    fn defaults_have_a_port_and_a_handshake_budget() {
        let o = DialOptions::new(ep());
        assert_eq!(o.endpoint.port, 2222);
        assert_eq!(o.handshake_timeout, Duration::from_secs(10));
        assert_eq!(o.keepalive, Some(Duration::from_secs(30)));
    }

    #[test]
    fn zero_keepalive_means_off_not_once_a_second() {
        assert_eq!(DialOptions::new(ep()).with_keepalive(0).keepalive, None);
    }

    #[test]
    fn client_id_is_well_formed_and_names_us_ourselves() {
        let cfg = DialOptions::new(ep()).config();
        // SshId 只有 Debug，没有 Clone/PartialEq，所以按引用模式匹配取内部 Cow
        let id: &str = match &cfg.client_id {
            SshId::Standard(id) => id,
            SshId::Raw(_) => panic!("我们用的是 Standard 标识，不该带自定义换行"),
        };
        assert!(id.starts_with("SSH-2.0-dbhx-shell_"), "{id}");
        // 协议对版本行有硬约束：不能有换行、不能超长
        assert!(!id.contains('\n') && id.len() < 255);
    }

    #[test]
    fn russh_wont_reap_the_connection_behind_the_pool() {
        let cfg = DialOptions::new(ep()).config();
        assert!(cfg.inactivity_timeout.is_none());
        assert!(cfg.nodelay);
    }

    #[test]
    fn pty_default_size_is_usable() {
        assert_eq!((PtySize::default().cols, PtySize::default().rows), (80, 24));
    }
}
