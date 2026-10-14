//! 命令层。按「前端菜单的形状」分文件：会话、文件面板、传输、主机库。
//!
//! 每个文件里的命令都是薄壳：真正的逻辑在 `*_impl`（只吃 `&Core`，不需要 Tauri 运行时），
//! `#[tauri::command]` 那层负责解 `State`、转参数、把错误翻成 [`IpcError`]。
//! 拆两层的唯一目的是**能在没有 webview 的测试里跑完全部命令**。

pub mod clipboard;
pub mod local;
pub mod session;
pub mod sftp;
pub mod transfer;
pub mod vault;

use std::time::{SystemTime, UNIX_EPOCH};

use dbhx_model::{ErrorCode, IpcError, LiveEndpoint, Pong};

/// 验证「命令入参含枚举 + 结构体返回 + TS 类型生成」闭环。
///
/// 这里的 `target` 是假数据：D1 还没有真正的会话层，只为把 `LiveEndpoint`
/// 推进类型生成器，确认它导出的 TS 形状符合预期。
#[tauri::command]
#[specta::specta]
pub async fn ping(seq: u32, echo: String) -> Result<Pong, IpcError> {
    Ok(Pong {
        seq,
        server_time_ms: now_ms(),
        echo,
        target: LiveEndpoint { host: "127.0.0.1".into(), port: 22, username: "probe".into() },
    })
}

/// 验证 `IpcError` 在错误通道上的序列化形状，以及枚举作为入参的可行性。
/// 永远返回 Err。
#[tauri::command]
#[specta::specta]
pub async fn ping_failure(code: ErrorCode, message: String) -> Result<Pong, IpcError> {
    let retryable = matches!(code, ErrorCode::Unreachable | ErrorCode::Timeout);
    Err(IpcError::new(code, message, retryable))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_returns_the_requested_seq_and_echo() {
        let pong = ping(7, "hello".into()).await.unwrap();
        assert_eq!(pong.seq, 7);
        assert_eq!(pong.echo, "hello");
        assert_eq!(pong.target.port, 22);
    }

    #[tokio::test]
    async fn unreachable_is_marked_retryable_but_auth_failed_is_not() {
        let a = ping_failure(ErrorCode::Unreachable, "no route".into()).await.unwrap_err();
        assert!(a.retryable);
        let b = ping_failure(ErrorCode::AuthFailed, "bad key".into()).await.unwrap_err();
        assert!(!b.retryable);
        assert_eq!(b.code, ErrorCode::AuthFailed);
    }
}
