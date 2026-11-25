//! vault 层的错误。与 IPC 的 `ErrorCode` 之间只有一处映射（`Into<IpcError>`），
//! 保证「解密失败」这种安全语义不会在边界上被降级成 `internal`。

use dbhx_model::{ErrorCode, IpcError};

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("vault document is malformed: {0}")]
    Malformed(String),
    /// 主密钥拿不到、密文被篡改、口令错——一律同一个分类，
    /// 且 message 不带任何密文字节，避免旁路泄露。
    #[error("vault could not be decrypted")]
    Crypto,
    #[error("{0} not found")]
    NotFound(String),
    /// 还有别的记录引用这条密文时不允许删
    #[error("{what} is still referenced by {count} record(s)")]
    InUse { what: String, count: usize },
    /// 平台没有可用的系统钥匙串（无桌面会话、CI 容器等）。调用方据此回退到文件源。
    #[error("system keyring is unavailable")]
    KeyringUnavailable,
    /// 钥匙串本身报错，或写入后读回的字节对不上。**不回退**：这类失败可能意味着
    /// 存进去的钥匙与库里那份不是同一把，悄悄退回文件只会把问题藏起来。
    #[error("system keyring operation failed: {0}")]
    Keyring(String),
}

impl From<VaultError> for IpcError {
    fn from(e: VaultError) -> Self {
        let code = match &e {
            VaultError::Io { .. } => ErrorCode::Io,
            VaultError::Malformed(_) => ErrorCode::Internal,
            // 解不开既不是 not_found 也不是 io：前端要按「库损坏/密钥不对」单独渲染
            VaultError::Crypto => ErrorCode::PermissionDenied,
            VaultError::NotFound(_) => ErrorCode::NotFound,
            VaultError::InUse { .. } => ErrorCode::PermissionDenied,
            // 钥匙串不可用是环境问题，不是用户操作错误
            VaultError::KeyringUnavailable => ErrorCode::Io,
            VaultError::Keyring(_) => ErrorCode::Internal,
        };
        IpcError::new(code, e.to_string(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_failure_maps_to_permission_denied_not_internal() {
        let ipc: IpcError = VaultError::Crypto.into();
        assert_eq!(ipc.code, ErrorCode::PermissionDenied);
        assert!(!ipc.retryable);
        // 错误文案里绝不能出现密文或密钥材料
        assert!(!ipc.message.to_lowercase().contains("key material"));
    }
}
