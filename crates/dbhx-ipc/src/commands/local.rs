//! 文件管理窗口：本机文件系统浏览 + 弹出独立窗口。
//!
//! 这里是唯一直接摸本机磁盘的命令层。之所以放会话层之外、单独成文件，是因为它
//! 与 SSH/vault 毫无关系——它读的是用户自己机器上的目录，不碰任何凭据，也不进
//! transport 池。前端的双栏文件窗口（本机 ↔ 远端）左半边就靠这一组命令。
//!
//! 两条铁律沿用全项目约定：
//! 1. **OS 的英文错误不外泄**。`std::io::Error` 的 Display 是英文（"Access is
//!    denied" 之类），一律只进 `tracing`；跨 IPC 的是按 [`std::io::ErrorKind`] 归好类
//!    的中文。
//! 2. **纯函数壳分离**。真逻辑在 `*_impl`（不吃 Tauri 运行时），`#[tauri::command]`
//!    那层只转发，这样全部命令能在无 webview 的单测里跑完。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use dbhx_model::{DirListing, ErrorCode, FileEntry, FileKind, HostId, IpcError};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// 文件管理窗口的 label 前缀。前端靠它判断「我是文件窗口还是主窗口」，并从后缀
/// 还原出该为哪台主机开远端栏——所以 label 必须只含 `HostId` 的 hex，不带别的。
pub const FILE_WINDOW_PREFIX: &str = "files-";

/// 把 `std::io::Error` 翻成不外泄英文的 [`IpcError`]，原文只进日志。
fn io_err(context: &str, e: std::io::Error) -> IpcError {
    tracing::debug!(context, %e, kind = ?e.kind(), "本机文件系统操作失败");
    let (code, msg) = match e.kind() {
        std::io::ErrorKind::NotFound => (ErrorCode::NotFound, "路径不存在"),
        std::io::ErrorKind::PermissionDenied => (ErrorCode::InvalidInput, "没有权限访问该路径"),
        std::io::ErrorKind::AlreadyExists => (ErrorCode::InvalidInput, "该名称已存在"),
        _ => (ErrorCode::Internal, "本机文件系统操作失败"),
    };
    IpcError::new(code, msg, false)
}

/// 家目录。Windows 认 `USERPROFILE`，类 Unix 认 `HOME`；两个都没有时返回 `None`，
/// 让调用方回一条「找不到主目录」而不是拿一个假路径去 `read_dir`。
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 根级入口列表。Windows 枚举实际存在的盘符（对 A..Z 探一次 `exists`，比引一个
/// 拿卷标的 crate 划算）；其余平台就一个 `/`。
pub fn drive_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|c| format!("{}:\\", c as char))
            .filter(|p| Path::new(p).exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

/// 列一个本机目录。目录排在文件前，各自按名字（忽略大小写）升序——与文件管理器习惯一致。
fn list_dir(path: &Path) -> Result<DirListing, IpcError> {
    let read = std::fs::read_dir(path).map_err(|e| io_err("read_dir", e))?;
    let mut entries: Vec<FileEntry> = Vec::new();
    for item in read.flatten() {
        let file_type = item.file_type().ok();
        let kind = match file_type {
            Some(t) if t.is_dir() => FileKind::Dir,
            Some(t) if t.is_symlink() => FileKind::Symlink,
            Some(t) if t.is_file() => FileKind::File,
            _ => FileKind::Other,
        };
        // 断链、竞态删除都会让 metadata 失败；那种情况按「未知大小 / 未知时间」显示。
        let meta = item.metadata().ok();
        let size = meta.as_ref().map(std::fs::Metadata::len).unwrap_or(0);
        let mtime_ms = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        entries.push(FileEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: item.path().display().to_string(),
            kind,
            size,
            mtime_ms,
            mode: None,
            owner: None,
            group: None,
        });
    }
    entries.sort_by(|a, b| {
        let dir_rank = |k: FileKind| if k == FileKind::Dir { 0 } else { 1 };
        dir_rank(a.kind)
            .cmp(&dir_rank(b.kind))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(DirListing { path: path.display().to_string(), entries })
}

/// 本机家目录路径。前端「主目录」按钮与初始定位都问它。
pub fn home_impl() -> Result<String, IpcError> {
    home_dir()
        .map(|p| p.display().to_string())
        .ok_or_else(|| IpcError::new(ErrorCode::NotFound, "找不到主目录", false))
}

/// 本机根级入口（盘符 / `/`）。
pub fn drives_impl() -> Vec<String> {
    drive_roots()
}

/// 列本机目录。`path` 为空时回到家目录。
pub fn list_impl(path: Option<String>) -> Result<DirListing, IpcError> {
    let target = match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home_dir().ok_or_else(|| IpcError::new(ErrorCode::NotFound, "找不到主目录", false))?,
    };
    list_dir(&target)
}

/// 本机新建目录（含多级）。
pub fn mkdir_impl(path: String) -> Result<(), IpcError> {
    std::fs::create_dir_all(&path).map_err(|e| io_err("create_dir", e))
}

/// 本机删除。目录递归删，文件直接删。
pub fn remove_impl(path: String, is_dir: bool) -> Result<(), IpcError> {
    let result = if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    result.map_err(|e| io_err("remove", e))
}

/// 本机改名/移动。`to` 是完整目标路径，交给 OS 决定跨卷等边界。
pub fn rename_impl(from: String, to: String) -> Result<(), IpcError> {
    std::fs::rename(&from, &to).map_err(|e| io_err("rename", e))
}

/// 弹出（或聚焦）某台主机的文件管理窗口。
///
/// 只建窗口、不带任何会话状态：新窗口是独立 webview，有自己的 Pinia，挂载后自己
/// 从 label 里的 `HostId` 去开远端栏。这里刻意不做「顺手把面板也开好」——那会让
/// 建窗口与拨号耦合，而拨号失败时用户至少还能看到本机栏。
pub fn open_file_window_impl(app: &AppHandle, host_id: HostId) -> Result<(), IpcError> {
    let label = format!("{FILE_WINDOW_PREFIX}{}", host_id.0);
    if let Some(window) = app.get_webview_window(&label) {
        // 同一台主机只留一个文件窗口；已存在就提到前面，而不是再开一个抢同一个连接。
        let _ = window.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title("dbhx 文件管理")
        .inner_size(1100.0, 700.0)
        .min_inner_size(760.0, 460.0)
        .resizable(true)
        .decorations(true)
        .build()
        .map_err(|e| {
            tracing::debug!(%e, "创建文件管理窗口失败");
            IpcError::new(ErrorCode::Internal, "无法打开文件管理窗口", false)
        })?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn local_home() -> Result<String, IpcError> {
    home_impl()
}

#[tauri::command]
#[specta::specta]
pub async fn local_drives() -> Vec<String> {
    drives_impl()
}

#[tauri::command]
#[specta::specta]
pub async fn local_list(path: Option<String>) -> Result<DirListing, IpcError> {
    list_impl(path)
}

#[tauri::command]
#[specta::specta]
pub async fn local_mkdir(path: String) -> Result<(), IpcError> {
    mkdir_impl(path)
}

#[tauri::command]
#[specta::specta]
pub async fn local_remove(path: String, is_dir: bool) -> Result<(), IpcError> {
    remove_impl(path, is_dir)
}

#[tauri::command]
#[specta::specta]
pub async fn local_rename(from: String, to: String) -> Result<(), IpcError> {
    rename_impl(from, to)
}

/// **必须是 `async`**：非 async 命令跑在事件循环线程上，而 `WebviewWindowBuilder::build()`
/// 会把建窗口这件事投递给事件循环再阻塞等回音——同一条线程等自己，一点就整窗卡死。
#[tauri::command]
#[specta::specta]
pub async fn open_file_window(app: AppHandle, host_id: HostId) -> Result<(), IpcError> {
    open_file_window_impl(&app, host_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbhx-local-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn home_is_absolute_and_drives_are_never_empty() {
        // 家目录在 CI 上未必存在内容，但一定解析得出；盘符列表也至少有一个根。
        assert!(!home_impl().unwrap().is_empty());
        assert!(!drives_impl().is_empty());
    }

    #[test]
    fn lists_dirs_before_files_sorted_by_name() {
        let dir = tempdir("sort");
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("Zeta")).unwrap();
        std::fs::write(dir.join("a.txt"), b"yy").unwrap();
        std::fs::create_dir(dir.join("alpha")).unwrap();

        let listing = list_impl(Some(dir.display().to_string())).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zeta", "a.txt", "b.txt"], "目录在前、各自忽略大小写排序");
        assert_eq!(listing.entries[0].kind, FileKind::Dir);
        assert_eq!(listing.entries[2].size, 2, "文件应带真实字节数");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_reports_not_found_in_chinese_not_english_os_text() {
        let err = list_impl(Some("Z:\\definitely-not-here-9f3a".into())).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert_eq!(err.message, "路径不存在", "OS 的英文报错绝不能跨 IPC");
    }

    #[test]
    fn mkdir_remove_rename_round_trip() {
        let dir = tempdir("mut");
        let sub = dir.join("made");
        mkdir_impl(sub.display().to_string()).unwrap();
        assert!(sub.is_dir());

        let file = dir.join("note.txt");
        std::fs::write(&file, b"hi").unwrap();
        let moved = dir.join("renamed.txt");
        rename_impl(file.display().to_string(), moved.display().to_string()).unwrap();
        assert!(moved.exists() && !file.exists());

        remove_impl(moved.display().to_string(), false).unwrap();
        remove_impl(sub.display().to_string(), true).unwrap();
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
