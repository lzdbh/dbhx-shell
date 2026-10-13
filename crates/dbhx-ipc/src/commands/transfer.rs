//! 批量传输命令：下载、上传、取消、查询在跑的批次。
//!
//! 传输不在这里编排——那是 `dbhx-sftp::Engine` 的事（并发上限、节流、取消语义、
//! 一个文件失败不拖垮整批，全在那边单测过）。这一层只做三件事：
//! 把前端给的路径解析成绝对路径、把计划摊平、把批次 id 交回去。
//!
//! 一条命令层的不变量：**批次 id 一旦返回，`Core` 就一直持有它**，
//! 直到批次自然结束或被 `transfer_cancel` 摘掉。前端不需要轮询「还在跑吗」，
//! 收尾由 [`crate::events::TransferBatchEnded`] 通知。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbhx_model::{ErrorCode, IpcError, TransferKind};
use dbhx_sftp::{Plan, SftpCopier, expand_local, expand_remote_paths, path};

use crate::state::Core;

/// 下载：远端勾选的若干项 → 本机目录。相对路径按面板当前目录解析。
pub async fn download_impl(core: &Arc<Core>, connection: u32, targets: Vec<String>, local_dir: String) -> Result<u32, IpcError> {
    if targets.is_empty() {
        return Err(nothing_selected());
    }
    let dest = Path::new(&local_dir);
    if !dest.is_absolute() {
        return Err(IpcError::new(ErrorCode::InvalidInput, "下载目标必须是绝对路径", false));
    }
    let shown = panel_shown(core, connection)?;
    let remotes: Vec<String> = targets
        .iter()
        .map(|t| if path::is_absolute(t) { t.clone() } else { path::join(&shown, t) })
        .collect();
    // 列举与传输走同一条通道的同一个 client 克隆：不另开连接，也不重新登录
    let lister = core.panel_lister(connection)?;
    let plan = expand_remote_paths(&lister, &remotes, dest).await?;
    start(core, connection, plan, TransferKind::Download)
}

/// 上传：本机路径（拖放或文件选择器给的绝对路径）→ 远端目录。
pub async fn upload_impl(core: &Arc<Core>, connection: u32, local_paths: Vec<String>, remote_dir: Option<String>) -> Result<u32, IpcError> {
    if local_paths.is_empty() {
        return Err(nothing_selected());
    }
    let dest = match remote_dir {
        Some(dir) if path::is_absolute(&dir) => dir,
        Some(dir) => path::join(&panel_shown(core, connection)?, &dir),
        None => panel_shown(core, connection)?,
    };
    let locals: Vec<PathBuf> = local_paths.iter().map(PathBuf::from).collect();
    let plan = expand_local(&locals, &dest)?;
    start(core, connection, plan, TransferKind::Upload)
}

/// 取消一批。已经在途的分块会在下一次边界停；排队中的条目直接不再起跑。
pub fn cancel_impl(core: &Core, batch: u32) -> Result<(), IpcError> {
    core.cancel_batch(batch)
}

/// 正在跑的批次（含发起它的面板）。前端重开传输列表时用一次，之后靠事件更新。
pub fn running_impl(core: &Core) -> Vec<crate::state::BatchInfo> {
    core.running_batches()
}

fn panel_shown(core: &Core, connection: u32) -> Result<String, IpcError> {
    core.panel_shown(connection)
        .ok_or_else(|| crate::state::not_found_panel(connection))
}

/// 摊平后的计划可能是空的（勾选的全是符号链接），这时不起跑、明确报错。
fn start(core: &Arc<Core>, connection: u32, plan: Plan, kind: TransferKind) -> Result<u32, IpcError> {
    // 锁屏期间不开新的传输批次（在跑的由覆盖层之下继续，锁不抽走已有连接）。
    core.require_unlocked()?;
    if plan.is_empty() {
        return Err(IpcError::new(
            ErrorCode::InvalidInput,
            "没有可搬运的内容：符号链接与特殊文件不传输",
            false,
        ));
    }
    let client = core.panel_client(connection)?;
    let copier = Arc::new(SftpCopier::new(client.session(), kind, core.engine.options().chunk_size));
    Ok(core.start_batch(connection, copier, plan))
}

fn nothing_selected() -> IpcError {
    IpcError::new(ErrorCode::InvalidInput, "没有勾选任何条目", false)
}

type State<'a> = tauri::State<'a, Arc<Core>>;

#[tauri::command]
#[specta::specta]
pub async fn sftp_download(core: State<'_>, connection: u32, targets: Vec<String>, local_dir: String) -> Result<u32, IpcError> {
    download_impl(&core, connection, targets, local_dir).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_upload(core: State<'_>, connection: u32, local_paths: Vec<String>, remote_dir: Option<String>) -> Result<u32, IpcError> {
    upload_impl(&core, connection, local_paths, remote_dir).await
}

#[tauri::command]
#[specta::specta]
pub fn transfer_cancel(core: State<'_>, batch: u32) -> Result<(), IpcError> {
    cancel_impl(&core, batch)
}

#[tauri::command]
#[specta::specta]
pub fn transfer_running(core: State<'_>) -> Result<Vec<crate::state::BatchInfo>, IpcError> {
    Ok(running_impl(&core))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_empty_selection_is_refused_before_touching_the_channel() {
        let (core, _rx) = crate::state::tests_harness("tr-empty", crate::state::fake_dialer());
        let err = download_impl(&core, 1, vec![], "/tmp/x".into()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(!err.retryable, "没勾东西，重试还是没东西");
    }

    #[tokio::test]
    async fn a_relative_download_target_is_rejected() {
        // 相对本机路径没有意义：进程的工作目录和用户的预期无关
        let (core, _rx) = crate::state::tests_harness("tr-relative", crate::state::fake_dialer());
        let err = download_impl(&core, 1, vec!["/logs".into()], "downloads".into()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("绝对路径"), "{}", err.message);
    }

    #[tokio::test]
    async fn transferring_without_a_panel_is_not_found() {
        let (core, _rx) = crate::state::tests_harness("tr-nopanel", crate::state::fake_dialer());
        let err = download_impl(&core, 42, vec!["/logs".into()], std::env::temp_dir().display().to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let err = upload_impl(&core, 42, vec!["/nope".into()], None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn cancelling_an_unknown_batch_is_not_found() {
        let (core, _rx) = crate::state::tests_harness("tr-cancel", crate::state::fake_dialer());
        let err = cancel_impl(&core, 9).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(!err.retryable, "批次已经结束，再取消一次不会让它回来");
        assert!(running_impl(&core).is_empty());
    }

    #[tokio::test]
    async fn a_missing_local_file_is_an_io_error_not_a_panic() {
        // 面板不存在时先报 NotFound；这里直接验证 expand_local 的错误能穿过 `?`
        let missing = std::env::temp_dir().join("dbhx-tr-definitely-absent");
        let err = match expand_local(&[missing], "/srv") {
            Ok(_) => panic!("本机路径不存在，规划不该成功"),
            Err(e) => IpcError::from(e),
        };
        assert_eq!(err.code, ErrorCode::Io, "{:?}", err.code);
    }
}
