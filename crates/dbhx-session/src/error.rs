//! 会话层错误。
//!
//! 只放「池与凭据来源」这一层的错。SSH 协议层与本地 PTY 的错误原样透传，
//! 因为它们已经按「UI 能做什么」分过类了，在这里再分一次只会分错。

use dbhx_model::{ErrorCode, IpcError};
use dbhx_pty::PtyError;
use dbhx_ssh::SshError;
use dbhx_telnet::TelnetError;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Ssh(#[from] SshError),

    /// 本地终端的错。与 SSH 一样，分类已经在 `dbhx-pty` 里做过，这里只透传。
    #[error(transparent)]
    Pty(#[from] PtyError),

    /// telnet 的错。分类已在 `dbhx-telnet` 里按「UI 能做什么」分好，这里只透传。
    #[error(transparent)]
    Telnet(#[from] TelnetError),

    /// 身份键指向的主机在 vault 里不存在（记录被删了，但还有 tab 拿着旧 id）。
    #[error("找不到主机记录: {0}")]
    UnknownHost(String),

    /// 记录在，凭据不在：没存口令、也没关联身份。
    #[error("该主机没有可用凭据: {0}")]
    NoCredential(String),

    /// 池里的连接已经不可用。与「拨号失败」区分开：前者重试有意义。
    #[error("连接已关闭")]
    Closed,

    /// 注册表里没有这个 id：tab 已关闭，或前端拿的是重启前的旧 id。
    #[error("会话不存在: {0}")]
    NoSuchSession(u32),

    /// 应用锁生效：所有会读凭据或发起连接的操作一律拒绝。
    /// 这是 UI/授权门，不是加密边界（主密钥仍在进程内），但锁屏期间必须硬拦拨号。
    #[error("应用已锁定")]
    Locked,

    /// 会话层自己的不变量被破坏，属于 bug。
    #[error("会话层内部错误: {0}")]
    Internal(String),
}

impl From<SessionError> for IpcError {
    fn from(e: SessionError) -> Self {
        match e {
            // 协议层的分类已经决定了 UI 该给什么按钮，不要在这里改写
            SessionError::Ssh(inner) => inner.into(),
            SessionError::Pty(inner) => inner.into(),
            SessionError::Telnet(inner) => inner.into(),
            SessionError::UnknownHost(id) => IpcError::new(
                ErrorCode::NotFound,
                format!("主机记录已不存在（{id}），请重新选择主机"),
                false,
            ),
            SessionError::NoCredential(label) => IpcError::new(
                ErrorCode::PermissionDenied,
                format!("{label} 没有可用凭据，请先在主机设置里填写口令或选择密钥"),
                false,
            ),
            SessionError::Closed => {
                IpcError::new(ErrorCode::ChannelClosed, "连接已关闭，请重新连接", true)
            }
            SessionError::NoSuchSession(id) => IpcError::new(
                ErrorCode::NotFound,
                format!("会话 {id} 已不存在，标签页可能已经关闭"),
                false,
            ),
            SessionError::Locked => IpcError::new(
                ErrorCode::Locked,
                "应用已锁定，解锁后才能连接",
                false,
            ),
            SessionError::Internal(_) => IpcError::internal("会话层内部错误"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_is_retryable_but_missing_credential_is_not() {
        let closed: IpcError = SessionError::Closed.into();
        assert_eq!(closed.code, ErrorCode::ChannelClosed);
        assert!(closed.retryable);

        let missing: IpcError = SessionError::NoCredential("db1".into()).into();
        assert_eq!(missing.code, ErrorCode::PermissionDenied);
        assert!(!missing.retryable, "重试不会凭空变出口令");
    }

    #[test]
    fn ssh_errors_keep_their_own_classification() {
        // 主机密钥变更是硬阻断，经过会话层之后仍然不可重试
        let e: IpcError = SessionError::Ssh(SshError::HostKeyChanged).into();
        assert_eq!(e.code, ErrorCode::HostKeyChanged);
        assert!(!e.retryable);
    }

    #[test]
    fn internal_message_never_carries_the_original_debug() {
        // Internal 的原文里可能有主机名甚至用户名，只留类别
        let e: IpcError = SessionError::Internal("lease underflow for root@10.0.0.1".into()).into();
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(!e.message.contains("10.0.0.1"), "{}", e.message);
    }
}
