//! russh `Handler` 实现。
//!
//! 这个 handler 跑在 russh 自己 spawn 的会话任务里，**不能**持有会话层的锁，
//! 所以一切对外通信都走 channel：主机密钥校验是「请求-应答」，其余是单向事件。
//!
//! 两条硬规矩：
//! * `check_server_key` 的 russh 默认实现一律拒绝，必须覆盖；
//! * 密钥校验必须有人回话。会话任务会卡在 `check_server_key` 里等，
//!   应答方（`transport` 的 actor）若先退出，oneshot 的发送端被 drop，
//!   我们这里收到 `Canceled` 并判定为拒绝——连接随之失败，这是期望行为而不是悬挂。

use std::sync::Arc;

use dbhx_model::LiveEndpoint;

use russh::client::{DisconnectReason, Handler, Session};
use russh::keys::PublicKey;
use russh::{ChannelId, keys::PublicKeyOrCertificate};
use tokio::sync::{mpsc, oneshot, watch};

use crate::hostkey::ServerKey;

/// 会话当前状态。用 watch 而不是事件，是为了**不丢信号**：
/// actor 可能在 `kex_done` 之后才来看，此时必须仍能读到已发生的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// TCP 已连上，密钥交换进行中
    Handshaking,
    /// 密钥交换完成且主机密钥被接受，可以做认证
    KeyExchanged,
    /// 认证通过
    Authenticated,
    /// 连接已结束（主动断开、远端断开或出错）
    Closed(CloseReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    ClientRequested,
    /// 主机密钥校验未通过。必须与「网络断了」区分开，否则 UI 会给出错误的重试建议
    HostKeyRejected,
    Server { reason_code: String, message: String },
    /// russh 会话任务返回的错误文本
    Failed(String),
}

impl SessionState {
    pub fn closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

/// 主机密钥校验请求：数据 + 回话用的 oneshot。
pub(crate) struct KeyCheck {
    pub key: ServerKey,
    pub reply: oneshot::Sender<bool>,
}

/// 往外抛的事件，UI 侧只关心这几类。
#[derive(Debug, Clone)]
pub enum SshEvent {
    /// 登录前服务器给的横幅（内网机器常写「请联系运维」之类）
    AuthBanner(String),
    /// 远端关闭了某个通道（多半是 shell 退出）
    ChannelClosed(ChannelId),
    /// 服务器 `hostkeys@openssh.com` 追加宣告的其它算法密钥，供「顺手补全 known_hosts」
    HostKeysAnnounced(Vec<ServerKey>),
}

pub(crate) struct ClientHandler {
    #[allow(dead_code)]
    endpoint: LiveEndpoint,
    state: Arc<watch::Sender<SessionState>>,
    events: mpsc::UnboundedSender<SshEvent>,
    checks: mpsc::UnboundedSender<KeyCheck>,
}

impl ClientHandler {
    pub(crate) fn new(
        endpoint: LiveEndpoint,
        state: Arc<watch::Sender<SessionState>>,
        events: mpsc::UnboundedSender<SshEvent>,
        checks: mpsc::UnboundedSender<KeyCheck>,
    ) -> Self {
        Self { endpoint, state, events, checks }
    }

    fn set_state(&self, next: SessionState) {
        let _ = self.state.send(next);
    }

    /// 密钥交换完成。已经认证过的连接在 rekey 后会再次走到这里，
    /// 绝不能把状态退回 `KeyExchanged`——上层正拿 `Authenticated` 当作开新通道的门禁。
    fn note_kex_done(&self) {
        if matches!(*self.state.borrow(), SessionState::Handshaking | SessionState::KeyExchanged) {
            self.set_state(SessionState::KeyExchanged);
        }
    }
}

impl Handler for ClientHandler {
    /// 用 russh 自己的错误类型：它已满足 `From<russh::Error> + Send + Debug`，
    /// 再包一层只会让 `connect()` 的返回类型变复杂而拿不到任何好处。
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let is_cert = matches!(server_public_key, PublicKeyOrCertificate::Certificate(_));
        let presented =
            ServerKey::from_public_key(&server_public_key.public_key(), is_cert)?;

        let (tx, rx) = oneshot::channel();
        if self.checks.send(KeyCheck { key: presented, reply: tx }).is_err() {
            self.set_state(SessionState::Closed(CloseReason::HostKeyRejected));
            return Ok(false);
        }
        match rx.await {
            Ok(true) => {
                self.note_kex_done();
                Ok(true)
            }
            _ => {
                self.set_state(SessionState::Closed(CloseReason::HostKeyRejected));
                Ok(false)
            }
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.events.send(SshEvent::AuthBanner(banner.to_string()));
        Ok(())
    }

    async fn kex_done(
        &mut self,
        _shared_secret: Option<&[u8]>,
        _names: &russh::Names,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 证书场景下 russh 可能不走我们的 check_server_key（CA 校验在库内完成），
        // 所以这里也要推状态。
        self.note_kex_done();
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.events.send(SshEvent::ChannelClosed(channel));
        Ok(())
    }

    async fn openssh_ext_host_keys_announced(
        &mut self,
        keys: Vec<PublicKey>,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let mut out = Vec::with_capacity(keys.len());
        for k in &keys {
            if let Ok(sk) = ServerKey::from_public_key(k, false) {
                out.push(sk);
            }
        }
        if !out.is_empty() {
            let _ = self.events.send(SshEvent::HostKeysAnnounced(out));
        }
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        // actor 不 await `Handle` 的 Future，所以错误不会自动传出来，
        // 必须在这里把状态写死，等待 state 变化的一方才能醒。
        let r = match &reason {
            DisconnectReason::ReceivedDisconnect(info) => CloseReason::Server {
                reason_code: format!("{:?}", info.reason_code),
                message: info.message.clone(),
            },
            DisconnectReason::Error(e) => CloseReason::Failed(e.to_string()),
        };
        self.set_state(SessionState::Closed(r));
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}

