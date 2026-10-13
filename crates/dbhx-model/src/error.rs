use serde::{Deserialize, Serialize};
use specta::Type;

/// 稳定的错误分类。前端只按 code 分支，永不解析 message。
///
/// 新增变体时必须同步 `frontend/src/lib/ipc/errors.ts` 的文案映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 认证被服务器拒绝（密码/密钥不匹配）
    AuthFailed,
    /// 首次见到的主机密钥，需要用户确认
    HostKeyUnknown,
    /// 主机密钥与已存记录不一致：中间人告警，UI 必须硬阻断且不可跳过
    HostKeyChanged,
    /// 堡垒机只允许单通道，无法复用 transport 开 SFTP
    SingleChannelBastion,
    /// 网络不可达 / DNS 失败 / 端口不通
    Unreachable,
    /// 超时
    Timeout,
    NotFound,
    PermissionDenied,
    /// 断点续传时源文件指纹不匹配
    ResumeMismatch,
    /// 超出并发或体积预算
    QuotaExceeded,
    Cancelled,
    /// transport 已关闭（常被 idle park 或远端断开触发）
    ChannelClosed,
    Io,
    Internal,
}

/// IPC 边界上唯一的错误类型。
///
/// 刻意不带 `detail: Option<serde_json::Value>` —— serde_json::Value 在 TS 侧只能导出成
/// `any`，会污染契约。需要结构化补充信息时，加一个具名 enum 而不是自由 JSON。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct IpcError {
    pub code: ErrorCode,
    /// 面向用户的说明。可能被本地化，不得用于逻辑判断。
    pub message: String,
    /// 提示 UI「重试」是否有可能成功
    pub retryable: bool,
}

impl IpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self { code, message: message.into(), retryable }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message, false)
    }

    pub fn auth_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::AuthFailed, message, false)
    }

    pub fn unreachable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unreachable, message, true)
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorCode::Cancelled, "cancelled", false)
    }
}

/// 约定：任何带秘密的 Debug 实现都必须走这里，避免凭据进日志。
pub fn redacted(what: &str) -> String {
    format!("<redacted:{what}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_serializes_to_snake_case_string() {
        assert_eq!(serde_json::to_string(&ErrorCode::HostKeyChanged).unwrap(), "\"host_key_changed\"");
    }

    #[test]
    fn ipc_error_round_trips_through_json() {
        let e = IpcError::unreachable("no route to 10.0.0.1:22");
        let json = serde_json::to_string(&e).unwrap();
        let back: IpcError = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, ErrorCode::Unreachable);
        assert!(back.retryable);
        assert!(json.contains("no route"));
    }

    #[test]
    fn redacted_never_leaks_the_value() {
        assert_eq!(redacted("private_key"), "<redacted:private_key>");
    }
}
