//! 会话日志命令：开始/停止把某个会话的终端输出抄一份到本地文件。
//!
//! 命令层只做「开与关」，真正的抄写发生在 [`crate::hub`] 那唯一的读者里——
//! 会话事件流只能有一个读者，在这里再订阅一次会随机丢一半字节。

use dbhx_model::{ErrorCode, IpcError, SessionId};

use crate::logsink::{self, LogSummary};
use crate::state::Core;

/// 开始记录。`dir` 缺省时落在应用数据目录的 `logs` 下。
///
/// 拒绝给一个已经不存在的 tab 开日志：那样只会留下一个空文件，用户以为它在记。
pub async fn start_impl(core: &Core, session: SessionId, dir: Option<&str>, file_name: &str) -> Result<LogSummary, IpcError> {
    let record = core.sessions.record(session).ok_or_else(|| {
        IpcError::new(ErrorCode::NotFound, format!("会话 {} 已经不存在", session.0), false)
    })?;
    let path = logsink::log_path(&core.dir, dir, file_name)?;
    if path.exists() && is_dir(&path) {
        return Err(IpcError::new(ErrorCode::InvalidInput, format!("{} 是一个目录", path.display()), false));
    }
    core.logs.start(session, path.clone()).await?;
    tracing::info!(session = session.0, label = %record.label, path = %path.display(), "开始记录会话日志");
    Ok(summary(path, 0))
}

/// 停止记录，回落点和已写字节数。
pub async fn stop_impl(core: &Core, session: SessionId) -> Result<LogSummary, IpcError> {
    let summary = core.logs.stop(session).await?;
    tracing::info!(bytes = summary.bytes, path = %summary.path, "会话日志已停止");
    Ok(summary)
}

fn summary(path: std::path::PathBuf, bytes: u64) -> LogSummary {
    LogSummary { path: path.to_string_lossy().into_owned(), bytes }
}

fn is_dir(path: &std::path::Path) -> bool {
    path.metadata().map(|m| m.is_dir()).unwrap_or(false)
}

type State<'a> = tauri::State<'a, std::sync::Arc<Core>>;

#[tauri::command]
#[specta::specta]
pub async fn session_log_start(
    core: State<'_>,
    session: SessionId,
    dir: Option<String>,
    file_name: String,
) -> Result<LogSummary, IpcError> {
    start_impl(&core, session, dir.as_deref(), &file_name).await
}

#[tauri::command]
#[specta::specta]
pub async fn session_log_stop(core: State<'_>, session: SessionId) -> Result<LogSummary, IpcError> {
    stop_impl(&core, session).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::PtySize;
    use std::path::Path;

    async fn local_session(tag: &str) -> (std::sync::Arc<Core>, SessionId) {
        let (core, _rx) = crate::state::tests_harness(tag, crate::state::fake_dialer());
        let spec = crate::commands::session::default_local_spec(None, PtySize::default());
        let info = crate::commands::session::open_local_impl(&core, spec, "本地".into()).await.unwrap();
        (core, info.id)
    }

    #[tokio::test]
    async fn output_written_after_start_lands_in_the_file() {
        let (core, session) = local_session("log-start").await;
        let summary = start_impl(&core, session, None, "会话日志.log").await.unwrap();
        // 命令层只负责开，字节是 hub 顺路抄的；这里直接调注册表模拟那一步
        let raw = "\u{1b}[31mFATAL\u{1b}[0m error\n";
        core.logs.tee(session, raw.as_bytes()).await.unwrap();
        let stopped = stop_impl(&core, session).await.unwrap();
        assert_eq!(stopped.path, summary.path);
        assert_eq!(stopped.bytes, raw.len() as u64);
        // 原样抄：转义序列一个字节都不动
        assert_eq!(std::fs::read_to_string(&stopped.path).unwrap(), raw);
        clean_log_dir(&core.dir);
    }

    #[tokio::test]
    async fn stopping_a_session_that_was_never_logged_reports_not_found() {
        let (core, session) = local_session("log-stop").await;
        let err = stop_impl(&core, session).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        clean_log_dir(&core.dir);
    }

    #[tokio::test]
    async fn a_closed_session_cannot_start_a_log() {
        // 否则只会留下一个永远不会有内容的空文件，而 UI 显示「正在记录」
        let (core, _rx) = crate::state::tests_harness("log-ghost", crate::state::fake_dialer());
        let err = start_impl(&core, SessionId(4242), None, "ghost.log").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_file_name_that_escapes_the_log_dir_is_refused() {
        let (core, session) = local_session("log-escape").await;
        for bad in ["../../evil.log", "F:/windows/x.log", ".."] {
            let err = start_impl(&core, session, None, bad).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "{bad} 不该被接受");
        }
        assert!(!Path::new("../evil.log").exists());
        clean_log_dir(&core.dir);
    }

    #[tokio::test]
    async fn starting_twice_on_one_session_is_refused() {
        let (core, session) = local_session("log-twice").await;
        start_impl(&core, session, None, "a.log").await.unwrap();
        let err = start_impl(&core, session, None, "b.log").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        stop_impl(&core, session).await.unwrap();
        clean_log_dir(&core.dir);
    }

    fn clean_log_dir(dir: &Path) {
        std::fs::remove_dir_all(dir.join("logs")).ok();
    }
}
