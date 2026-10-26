//! SSH 层错误。
//!
//! 分类以「UI 能做什么」为准：能重试的、要换凭据的、必须硬阻断的，三类。
//! 到 IPC 边界一律翻成 `IpcError`，`code` 是前端唯一允许分支的字段。

use dbhx_model::{ErrorCode, IpcError};

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("无法连接: {0}")]
    Unreachable(String),

    #[error("连接或握手超时")]
    Timeout,

    /// 首次接触这台主机。只有在**无人可问**的场景（免登录复用、后台重连）才会成为错误。
    #[error("主机密钥未记录")]
    HostKeyUnknown,

    /// 与已存记录不一致：中间人风险，任何情况下都不给「仍然连接」。
    #[error("主机密钥与已存记录不一致")]
    HostKeyChanged,

    #[error("认证失败")]
    AuthFailed,

    /// 服务器在 `keyboard-interactive` 里要求的不止一个口令（典型是 2FA）。
    /// M1 不代填，交给上层下一步处理。
    #[error("服务器要求交互式认证，需要用户输入")]
    InteractiveAuthRequired,

    #[error("密钥或口令无法解析: {0}")]
    KeyLoad(String),

    /// 服务器拒绝了某个通道请求（pty / shell / exec / subsystem）。
    #[error("服务器拒绝了 {request}")]
    RequestRefused { request: &'static str },

    /// 参数在本地就能判定不合法（空用户名、超长口令）。发出去只会浪费一次远端往返。
    #[error("输入不合法: {0}")]
    InvalidInput(String),

    /// sudo 提权没跑起来。`message` 已经是给用户看的中文（见 `sudo_sftp::classify_elevate_failure`）。
    ///
    /// 这一类**绝不能**被上层当成「那就用普通身份继续」：用户点的是提权，
    /// 悄悄降级等于让他以为自己在管 root 的文件目录。
    #[error("{0}")]
    ElevateDenied(String),

    #[error("连接已关闭")]
    Closed,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Russh(#[from] russh::Error),
}

impl SshError {
    pub fn refused(request: &'static str) -> Self {
        Self::RequestRefused { request }
    }
}

impl From<SshError> for IpcError {
    fn from(e: SshError) -> Self {
        match e {
            SshError::Unreachable(r) => IpcError::unreachable(r),
            SshError::Timeout => IpcError::new(ErrorCode::Timeout, "连接超时", true),
            SshError::HostKeyUnknown => IpcError::new(
                ErrorCode::HostKeyUnknown,
                "首次连接该主机，需要确认其主机密钥",
                false,
            ),
            SshError::HostKeyChanged => IpcError::new(
                ErrorCode::HostKeyChanged,
                "主机密钥与已记录的不一致，连接已被阻止",
                false,
            ),
            // 认证失败的 message 由调用方补（服务器可能给出 banner），这里只保证 code 正确
            SshError::AuthFailed => IpcError::auth_failed("用户名或密码/密钥不正确"),
            SshError::InteractiveAuthRequired => {
                IpcError::new(ErrorCode::AuthFailed, "服务器要求交互式认证", false)
            }
            SshError::KeyLoad(r) => {
                IpcError::new(ErrorCode::PermissionDenied, r, false)
            }
            SshError::RequestRefused { request } => {
                IpcError::new(ErrorCode::ChannelClosed, format!("服务器拒绝 {request}"), true)
            }
            SshError::InvalidInput(r) => IpcError::new(ErrorCode::InvalidInput, r, false),
            // 提权失败的原因已经在本层翻成中文，重试同一条命令不会改变远端 sudoers，
            // 所以 retryable=false：UI 只该给用户「换个用户/口令再试」的入口。
            SshError::ElevateDenied(r) => IpcError::new(ErrorCode::PermissionDenied, r, false),
            SshError::Closed => IpcError::new(ErrorCode::ChannelClosed, "连接已关闭", true),
            SshError::Io(io) => IpcError::new(ErrorCode::Io, io.to_string(), true),
            // russh 的内部错误文本可能带主机名甚至用户名，只透出类型不带原文
            SshError::Russh(russh::Error::NotAuthenticated) => IpcError::auth_failed("尚未认证"),
            SshError::Russh(other) => {
                tracing::debug!(error = %other, "russh 内部错误");
                IpcError::internal("SSH 协议错误")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_changed_is_not_retryable() {
        let e: IpcError = SshError::HostKeyChanged.into();
        assert_eq!(e.code, ErrorCode::HostKeyChanged);
        assert!(!e.retryable, "密钥变更重试没有意义，UI 不能给重试按钮");
    }

    #[test]
    fn unreachable_is_retryable_but_auth_failed_is_not() {
        assert!(IpcError::from(SshError::Unreachable("refused".into())).retryable);
        assert!(!IpcError::from(SshError::AuthFailed).retryable);
    }

    #[test]
    fn elevate_denied_keeps_the_reason_and_offers_no_retry() {
        // 「远端 sudo 不允许」是这句话本身，不能被翻成一句通用的权限错误——
        // 用户需要知道该去改 sudoers 而不是再点一次。
        let e: IpcError = SshError::ElevateDenied("远端 sudo 不允许当前用户使用 sudo，提权失败".into()).into();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.message.contains("sudo"), "{}", e.message);
        assert!(!e.retryable, "重试同一条 sudo 命令不会改变远端 sudoers");
    }

    #[test]
    fn invalid_input_is_not_retryable() {
        let e: IpcError = SshError::InvalidInput("提权口令不能为空".into()).into();
        assert_eq!(e.code, ErrorCode::InvalidInput);
        assert!(!e.retryable);
    }

    #[test]
    /// russh 的错误原文里有 peer 信息，绝不能跨过 IPC 泄漏给 webview。
    fn russh_errors_never_leak_their_original_text() {
        let e: IpcError = SshError::Russh(russh::Error::Inconsistent).into();
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(!e.message.contains("Inconsistent"), "{}", e.message);
    }
}
