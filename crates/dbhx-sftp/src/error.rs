//! SFTP 层错误与到 IPC 错误的映射。
//!
//! 这一层最容易写错的地方是「把服务端的英文原文透出去」。SFTP 的 `STATUS` 报文里带一句
//! 服务端自述的 `error_message`（OpenSSH 给的是英文，某些国产设备给的是 GBK 乱码字节），
//! russh-sftp 的 `Display` 就是 `"{status_code}: {error_message}"`。直接 `to_string()`
//! 会带来两个问题：文案不可本地化、乱码进 UI。所以这里**只取 `status_code`**，
//! 路径由调用方在分类点补上（调用方知道自己在动哪个文件，服务端那句话未必说得对）。

use dbhx_model::{ErrorCode, IpcError};
use russh_sftp::client::error::Error as ProtocolError;
use russh_sftp::protocol::StatusCode;

/// SFTP 操作失败。
#[derive(Debug, Clone, thiserror::Error)]
pub enum SftpError {
    /// 远端没有这个路径
    #[error("路径不存在: {0}")]
    NotFound(String),

    /// 服务端拒绝（权限、只读文件系统、配额之外的一律算这里）
    #[error("没有权限执行该操作: {0}")]
    PermissionDenied(String),

    /// 目标已存在。SFTP 的 `rename`/`mkdir` 在多数服务端不会覆盖，而是直接报这条
    #[error("目标已存在: {0}")]
    AlreadyExists(String),

    /// 路径写法本身有问题（本地校验就没过），不可能是网络原因
    #[error("路径不合法: {0}")]
    BadPath(String),

    /// 子系统流断了：连接被回收、远端把 channel 关了，或 SSH 层出错
    #[error("文件通道已关闭")]
    Closed,

    /// 请求超时。与「通道断了」区分开：超时可以立刻重试
    #[error("操作超时")]
    Timeout,

    /// 读写本身失败（磁盘满、远端 I/O error）。可重试性由 UI 按上下文判断
    #[error("数据传输失败: {0}")]
    Io(String),

    /// 服务端不支持或行为不符合协议
    #[error("服务端不支持该操作: {0}")]
    Unsupported(String),

    /// 本地文件系统错误。带上路径，用户看得懂是哪个文件
    #[error("本地文件错误: {path}: {reason}")]
    LocalIo { path: String, reason: String },

    /// 用户取消
    #[error("已取消")]
    Cancelled,

    /// 续传前提不成立（远端文件在这期间变了）
    #[error("远端文件已变化，续传不成立")]
    ResumeMismatch,

    /// 超出并发或体积预算
    #[error("超出传输并发预算: {0}")]
    Quota(String),

    /// 分类不到别处、且属于我们的 bug
    #[error("SFTP 内部错误")]
    Internal(String),
}

impl SftpError {
    /// 把协议错误按 `status_code` 归类。
    ///
    /// `where_` 是本次操作的路径或目的地的简短描述，只用于给用户定位「哪个文件出事」，
    /// 不参与逻辑判断。
    pub fn classify(err: ProtocolError, where_: impl Into<String>) -> Self {
        let where_ = where_.into();
        match err {
            ProtocolError::Status(status) => Self::from_status(status.status_code, where_),
            ProtocolError::Timeout => Self::Timeout,
            ProtocolError::IO(_) => Self::Io(where_),
            ProtocolError::Limited(_) => Self::Quota(where_),
            // 这两类都是「对端行为不符合协议」，重试没有意义，报不支持
            ProtocolError::UnexpectedPacket | ProtocolError::UnexpectedBehavior(_) => {
                Self::Unsupported(where_)
            }
        }
    }

    fn from_status(code: StatusCode, where_: String) -> Self {
        // 逐个码明确归类，不写通配臂：协议里新增码时编译器会提醒我们决定它算哪类
        match code {
            // Ok 出现在错误分支意味着上层逻辑串了包，属于 bug
            StatusCode::Ok => Self::Internal(where_),
            StatusCode::Eof => Self::Io(where_),
            StatusCode::NoSuchFile => Self::NotFound(where_),
            StatusCode::PermissionDenied => Self::PermissionDenied(where_),
            // Failure 是服务端的兜底码，最常见的来源是磁盘满与配额，重试有可能成功
            StatusCode::Failure => Self::Io(where_),
            StatusCode::BadMessage => Self::Internal(where_),
            StatusCode::NoConnection | StatusCode::ConnectionLost => Self::Closed,
            StatusCode::OpUnsupported => Self::Unsupported(where_),
        }
    }

    /// 本地 IO 错误的统一入口：只保留路径与 OS 给的短说明。
    pub fn local_io(path: &str, err: &std::io::Error) -> Self {
        Self::LocalIo { path: path.to_string(), reason: io_kind_of(err) }
    }
}

/// `std::io::Error` 的 `Display` 在不同平台措辞不同（Windows 还会带系统错误码），
/// 只取 `kind` 既稳定又能本地化。
fn io_kind_of(err: &std::io::Error) -> String {
    let kind = match err.kind() {
        std::io::ErrorKind::NotFound => "文件不存在",
        std::io::ErrorKind::PermissionDenied => "权限不足",
        std::io::ErrorKind::AlreadyExists => "已存在",
        std::io::ErrorKind::InvalidInput => "参数不合法",
        std::io::ErrorKind::UnexpectedEof => "文件意外结束",
        std::io::ErrorKind::Interrupted => "操作被中断",
        std::io::ErrorKind::BrokenPipe => "管道已关闭",
        other => return format!("{other:?}"),
    };
    kind.to_string()
}

impl From<SftpError> for IpcError {
    fn from(e: SftpError) -> Self {
        match e {
            SftpError::NotFound(path) => IpcError::new(ErrorCode::NotFound, format!("路径不存在：{path}"), false),
            SftpError::PermissionDenied(path) => {
                IpcError::new(ErrorCode::PermissionDenied, format!("没有权限执行该操作：{path}"), false)
            }
            SftpError::AlreadyExists(path) => {
                IpcError::new(ErrorCode::AlreadyExists, format!("目标已存在：{path}"), false)
            }
            SftpError::BadPath(path) => {
                IpcError::new(ErrorCode::InvalidInput, format!("路径不合法：{path}"), false)
            }
            SftpError::Closed => IpcError::new(ErrorCode::ChannelClosed, "文件通道已关闭，请重新打开面板", true),
            SftpError::Timeout => IpcError::new(ErrorCode::Timeout, "操作超时，可重试", true),
            SftpError::Io(where_) => IpcError::new(ErrorCode::Io, format!("数据传输失败：{where_}"), true),
            SftpError::Unsupported(where_) => {
                IpcError::new(ErrorCode::Io, format!("服务端不支持该操作：{where_}"), false)
            }
            SftpError::LocalIo { path, reason } => {
                IpcError::new(ErrorCode::Io, format!("本地文件错误（{path}）：{reason}"), false)
            }
            SftpError::Cancelled => IpcError::cancelled(),
            SftpError::ResumeMismatch => {
                IpcError::new(ErrorCode::ResumeMismatch, "远端文件已变化，需重新传输", false)
            }
            SftpError::Quota(where_) => {
                IpcError::new(ErrorCode::QuotaExceeded, format!("超出传输并发预算：{where_}"), true)
            }
            // Internal 的原文可能带用户名/路径片段，只留类别
            SftpError::Internal(_) => IpcError::internal("SFTP 内部错误"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::Status;

    /// 服务端原文一律是这种带英文的报文，测试的靶子就是「它不能出现在出口」。
    fn status(code: StatusCode, english: &str) -> ProtocolError {
        ProtocolError::Status(Status {
            id: 1,
            status_code: code,
            error_message: english.to_string(),
            language_tag: "en".to_string(),
        })
    }

    #[test]
    fn missing_path_becomes_not_found_without_the_server_text() {
        let e = SftpError::classify(
            status(StatusCode::NoSuchFile, "No such file or directory"),
            "/var/log/app.log",
        );
        assert!(matches!(&e, SftpError::NotFound(p) if p == "/var/log/app.log"));
        let ipc: IpcError = e.into();
        assert_eq!(ipc.code, ErrorCode::NotFound);
        assert!(!ipc.retryable, "路径不存在不是网络问题");
        assert!(!ipc.message.contains("No such file"), "{}", ipc.message);
    }

    #[test]
    fn lost_channel_is_retryable_but_denied_is_not() {
        let closed: IpcError = SftpError::classify(status(StatusCode::ConnectionLost, "Connection lost"), "/x").into();
        assert_eq!(closed.code, ErrorCode::ChannelClosed);
        assert!(closed.retryable, "重开面板就能再连");

        let denied: IpcError =
            SftpError::classify(status(StatusCode::PermissionDenied, "Permission denied"), "/etc/shadow").into();
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
        assert!(!denied.retryable, "重试不会变出权限");
        assert!(denied.message.contains("/etc/shadow"));
    }

    #[test]
    fn timeout_and_io_are_told_apart() {
        assert!(matches!(SftpError::classify(ProtocolError::Timeout, "/x"), SftpError::Timeout));
        let io: IpcError = SftpError::classify(ProtocolError::IO("disk full".into()), "/data/big.bin").into();
        assert_eq!(io.code, ErrorCode::Io);
        assert!(io.retryable);
        assert!(!io.message.contains("disk full"), "russh 拼的英文不该出现在出口：{}", io.message);

        let timeout: IpcError = SftpError::Timeout.into();
        assert_eq!(timeout.code, ErrorCode::Timeout);
        assert!(timeout.retryable);
    }

    #[test]
    fn protocol_violations_are_not_reported_as_network_faults() {
        let e: IpcError =
            SftpError::classify(ProtocolError::UnexpectedBehavior("bad name packet".into()), "/x").into();
        assert_eq!(e.code, ErrorCode::Io);
        assert!(!e.retryable, "服务端行为不合协议，重试只是再错一次");
        assert!(!e.message.contains("bad name packet"), "{}", e.message);
    }

    #[test]
    fn limits_extension_quota_maps_to_quota_exceeded() {
        let e: IpcError = SftpError::classify(ProtocolError::Limited("Packet too large".into()), "big.bin").into();
        assert_eq!(e.code, ErrorCode::QuotaExceeded);
        assert!(!e.message.contains("Packet too large"), "{}", e.message);
    }

    #[test]
    fn unexpected_behavior_message_is_the_one_text_we_never_forward() {
        // 这一条是整份分类逻辑的存在理由：russh-sftp 的 Display 会把服务端原文拼进来，
        // 而我们的出口只认 status code
        let leaky = ProtocolError::UnexpectedBehavior("Permission denied by bastion policy".to_string());
        assert!(format!("{leaky}").contains("bastion"), "上游的英文在这里，别弄丢前提");
        let e: IpcError = SftpError::classify(leaky, "/var/log/app.log").into();
        assert!(!e.message.contains("bastion"), "{}", e.message);
        assert!(e.message.contains("/var/log/app.log"), "路径是唯一允许转述的细节：{}", e.message);
    }

    #[test]
    fn internal_never_carries_the_original_text() {
        let e: IpcError = SftpError::Internal("root@10.0.0.1 /home/root/.ssh".into()).into();
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(!e.message.contains("10.0.0.1"), "{}", e.message);
        assert!(!e.message.contains("root"), "{}", e.message);
    }

    #[test]
    fn local_io_keeps_the_path_but_not_the_platform_wording() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "os says nope");
        let e = SftpError::local_io("C:/Downloads/a.bin", &err);
        let ipc: IpcError = e.into();
        assert_eq!(ipc.code, ErrorCode::Io);
        assert!(ipc.message.contains("C:/Downloads/a.bin"), "{}", ipc.message);
        assert!(ipc.message.contains("权限不足"), "{}", ipc.message);
        assert!(!ipc.message.contains("os says nope"), "{}", ipc.message);
    }

    #[test]
    fn cancelled_is_cancelled() {
        let ipc: IpcError = SftpError::Cancelled.into();
        assert_eq!(ipc.code, ErrorCode::Cancelled);
        assert!(!ipc.retryable);
    }
}
