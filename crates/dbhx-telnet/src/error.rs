//! telnet 层错误。分类就按「UI 能做什么」来，别在这里再分一次。

use dbhx_model::{ErrorCode, IpcError};

/// telnet 客户端的错误。
#[derive(Debug, thiserror::Error)]
pub enum TelnetError {
    /// 连接、读写层面的 IO 错误。DNS 失败、端口不通、连接被重置都归到这里。
    ///
    /// 原始 OS 文本（多半是英文）只进 `tracing`，跨 IPC 的永远是人话中文——
    /// 与 SSH/PTY 层同一约定。
    #[error("无法连接 telnet 服务器")]
    Io(#[from] std::io::Error),

    /// 会话已结束（对端断开或本地关闭），再往里写东西就是写进空气。
    #[error("telnet 会话已关闭")]
    Closed,
}

impl From<TelnetError> for IpcError {
    fn from(e: TelnetError) -> Self {
        // 先取 Display 文本（是人话中文），再按 move 解构分类——顺序反了就借用已移动的值
        let message = e.to_string();
        match e {
            // 端口不通/DNS 失败：换个网络或服务器重启后就可能连上，值得重试
            TelnetError::Io(inner) => {
                let retryable = inner.kind() != std::io::ErrorKind::NotFound;
                // 原始 OS 文本多半是英文，只进 tracing，绝不跨 IPC
                tracing::debug!(%inner, "telnet 连接失败");
                IpcError::new(ErrorCode::Unreachable, message, retryable)
            }
            // 会话没了：与 SSH/PTY 的「已关闭」同一 code，前端只有一处分支
            TelnetError::Closed => IpcError::new(ErrorCode::ChannelClosed, message, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_failure_is_unreachable_and_never_carries_the_os_text() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "Connection refused (os error 111)");
        let e: IpcError = TelnetError::Io(io).into();
        assert_eq!(e.code, ErrorCode::Unreachable);
        assert!(e.retryable, "端口不通，服务器起来后重试就有救");
        assert!(!e.message.contains("os error"), "{} —— 原始英文 OS 文本不得跨 IPC", e.message);
    }

    #[test]
    fn dns_not_found_is_not_retryable() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such host");
        let e: IpcError = TelnetError::Io(io).into();
        assert_eq!(e.code, ErrorCode::Unreachable);
        assert!(!e.retryable, "主机名解析不出来，重试也是白试");
    }

    #[test]
    fn closed_matches_the_other_layers_channel_code() {
        let e: IpcError = TelnetError::Closed.into();
        assert_eq!(e.code, ErrorCode::ChannelClosed);
        assert!(e.retryable);
    }
}
