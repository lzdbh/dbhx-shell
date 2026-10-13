use dbhx_model::{ErrorCode, IpcError};

/// 本地 PTY 的错误。
///
/// 与 SSH 侧同一套约定：`message` 是给运维看的可行动中文，
/// 库的原始英文（anyhow / OS 文本）只进 `tracing`，不跨 IPC。
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// 伪终端本身没开出来（ConPTY / forkpty 失败，或达到系统句柄上限）
    #[error("无法创建伪终端")]
    Open,
    /// pty 开成功了但子进程没起来：路径错、不是可执行文件、权限不足
    #[error("无法启动子进程: {program}")]
    Spawn { program: String },
    /// 参数组合不合法，例如只给了参数没给程序
    #[error("启动参数不合法: {0}")]
    BadSpec(String),
    /// cwd 不存在。必须显式报错：portable-pty 对不存在的目录会静默改用
    /// `USERPROFILE`，用户只会看到「目录设置没生效」。
    #[error("工作目录不存在: {0}")]
    BadCwd(String),
    /// 会话已结束（子进程退出或句柄被回收）
    #[error("会话已关闭")]
    Closed,
    /// 输入队列满。子进程停止读取时才会出现，丢弃输入比阻塞 UI 好。
    #[error("终端输入队列已满，子进程可能没有在读取输入")]
    Busy,
}

impl From<PtyError> for IpcError {
    fn from(e: PtyError) -> Self {
        // code 与 dbhx-ssh 对齐：同一类「会话没了」在前端只有一处分支。
        let code = match e {
            PtyError::Open => ErrorCode::Io,
            PtyError::Closed => ErrorCode::ChannelClosed,
            PtyError::Spawn { .. } => ErrorCode::NotFound,
            PtyError::BadSpec(_) => ErrorCode::Internal,
            PtyError::BadCwd(_) => ErrorCode::NotFound,
            PtyError::Busy => ErrorCode::QuotaExceeded,
        };
        // 除了参数/目录写错，其它三种重试都可能有救：资源紧张、进程已死、子进程恢复读取。
        let retryable = !matches!(e, PtyError::BadSpec(_) | PtyError::BadCwd(_));
        IpcError::new(code, e.to_string(), retryable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_cwd_is_not_found_and_not_retryable() {
        let e: IpcError = PtyError::BadCwd("D:/nope".into()).into();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(!e.retryable);
        assert!(e.message.contains("D:/nope"), "{}", e.message);
    }

    #[test]
    fn full_input_queue_is_retryable() {
        let e: IpcError = PtyError::Busy.into();
        assert_eq!(e.code, ErrorCode::QuotaExceeded);
        assert!(e.retryable);
    }

    #[test]
    fn closed_session_matches_the_ssh_layer_code() {
        let e: IpcError = PtyError::Closed.into();
        assert_eq!(e.code, ErrorCode::ChannelClosed);
        assert!(e.retryable, "重开一个本地终端就是这里的「重试」");
    }

    #[test]
    fn missing_program_is_not_found() {
        let e: IpcError = PtyError::Spawn { program: "nvim".into() }.into();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(e.message.contains("nvim"), "{}", e.message);
    }
}
