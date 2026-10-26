//! 文件管理窗口：本机文件系统浏览 + 弹出独立窗口。
//!
//! 这里是唯一直接摸本机磁盘的命令层。之所以放会话层之外、单独成文件，是因为它
//! 与 SSH/vault 毫无关系——它读的是用户自己机器上的目录，不碰任何凭据，也不进
//! transport 池。前端的双栏文件窗口（本机 ↔ 远端）左半边就靠这一组命令，
//! 设置页那两个目录输入框旁边的「选择…」也是（`local_pick_dir` 弹原生对话框）。
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
        entries.push(entry_of(&item.path(), kind, meta.as_ref()));
    }
    entries.sort_by(|a, b| {
        let dir_rank = |k: FileKind| if k == FileKind::Dir { 0 } else { 1 };
        dir_rank(a.kind)
            .cmp(&dir_rank(b.kind))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(DirListing { path: path.display().to_string(), entries })
}

/// 把本机路径拼成条目。`meta` 缺省时大小/时间留空而不是填 0——
/// 「服务端没给」与「真的是 0」在 UI 上是两回事，本机同理。
fn entry_of(target: &Path, kind: FileKind, meta: Option<&std::fs::Metadata>) -> FileEntry {
    let size = meta.map(std::fs::Metadata::len).unwrap_or(0);
    let mtime_ms = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    FileEntry {
        name: target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| target.display().to_string()),
        path: target.display().to_string(),
        kind,
        size,
        mtime_ms,
        mode: None,
        owner: None,
        group: None,
    }
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

/// 弹原生的「选择文件夹」对话框，回用户选中的绝对路径。取消就是 `None`，不是错误。
///
/// 起始目录优先用调用方传的那个——设置页把当前已存的值原样送进来，改目录时对话框就开在
/// 用户正看着的地方；没传则回到家目录。路径不存在时不用额外判断：rfd 会自己回落到默认位置。
///
/// **必须 `async`**：这条要等用户在对话框里点下「选择」或「取消」为止，同步命令会把整条
/// 事件循环钉在那儿——所有窗口的输入都停。
pub async fn pick_dir_impl(default_path: Option<String>) -> Option<String> {
    let start = default_path
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(home_dir);
    let mut dialog = rfd::AsyncFileDialog::new().set_title("选择目录");
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder().await.map(|picked| picked.path().display().to_string())
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

/// 单个路径的元数据。外部编辑流程靠它轮询临时副本有没有被别的程序改过。
///
/// 与目录列举不同，这里**跟随符号链接**：用户要编辑的是链接指向的真实文件，
/// 拿链接自身的元数据轮询会永远看不到大小变化。
pub fn stat_impl(path: String) -> Result<FileEntry, IpcError> {
    let target = Path::new(&path);
    let meta = std::fs::metadata(target).map_err(|e| io_err("stat", e))?;
    let kind = if meta.is_dir() { FileKind::Dir } else if meta.is_file() { FileKind::File } else { FileKind::Other };
    Ok(entry_of(target, kind, Some(&meta)))
}

/// 本机新建空文件。
///
/// 存在性判断与创建之间有一个 TOCTOU 窗口（本机同一时刻只有一个文件窗口在动，
/// 外部程序挤进来的概率极低），真撞上了 `File::create` 会覆盖——那是并发写同一文件
/// 的通用后果，不是这里能兜住的，兜它要付出的是每次建文件都多一次打开。
pub fn create_file_impl(path: String) -> Result<(), IpcError> {
    if Path::new(&path).exists() {
        return Err(IpcError::new(ErrorCode::InvalidInput, "该名称已存在", false));
    }
    std::fs::File::create(&path).map(|_| ()).map_err(|e| io_err("create_file", e))
}

/// 编辑副本的「还算在用」期限。前端一次编辑最多监听三十分钟，留一倍余量：
/// 超过它的必然是被丢下的那份。
const EDIT_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// 外部编辑用的临时目录（`<系统临时目录>/dbhx-edit`），不存在就建出来。
///
/// 远端文件的副本放这儿，编辑完由文件窗口比对 mtime/大小后回传。刻意不放下载目录——
/// 那是用户挑来收文件的地方，塞一堆编辑副本进去等于污染。
pub fn edit_dir_impl() -> Result<String, IpcError> {
    let dir = std::env::temp_dir().join("dbhx-edit");
    std::fs::create_dir_all(&dir).map_err(|e| io_err("edit_dir", e))?;
    prune_stale_edits(&dir, EDIT_STALE_AFTER);
    Ok(dir.display().to_string())
}

/// 扫掉上一次被丢下的编辑副本。
///
/// 正常的收尾是前端在 `finally` 里删临时文件的，但用户直接关掉文件窗口时 webview
/// 连同 store 一起销毁，那个 `finally` 根本没机会跑完——副本就长留在临时目录里。
/// 前端没有可靠的地方补这一刀（它自己也一起死了），只能靠这个「下次再编辑时先打扫」
/// 的后端入口。按 mtime 设期限，正在编辑的那份还年轻，不会被误删。
///
/// 全程尽力而为：读目录失败、某个文件删不掉，一律忽略——清理是附带好处，
/// 不该让它挡住「拿到编辑目录」这件正事。
fn prune_stale_edits(dir: &Path, max_age: std::time::Duration) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in read.flatten() {
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mtime| now.duration_since(mtime).ok());
        if matches!(age, Some(a) if a > max_age) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::debug!(%e, "编辑副本没删掉");
            }
        }
    }
}

/// 交给系统关联的程序打开（记事本、VS Code、图片查看器……）。
///
/// Windows 走 `explorer.exe <路径>` 而不是 `cmd /C start`：后者会把命令行再解析一遍，
/// 文件名里的 `&` 和引号就成了命令片段。explorer 成功时也常回非零码，所以只看
/// 「进程有没有起来」，不判状态码。
pub fn open_with_impl(path: String) -> Result<(), IpcError> {
    let spawned = {
        #[cfg(windows)]
        let it = std::process::Command::new("explorer.exe").arg(&path).spawn();
        #[cfg(target_os = "macos")]
        let it = std::process::Command::new("open").arg(&path).spawn();
        #[cfg(not(any(windows, target_os = "macos")))]
        let it = std::process::Command::new("xdg-open").arg(&path).spawn();
        it
    };
    match spawned {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::debug!(%e, "调用系统关联程序失败");
            Err(IpcError::new(ErrorCode::Internal, "无法启动打开该文件的程序", false))
        }
    }
}

/// 弹出（或聚焦）某台主机的文件管理窗口。
///
/// 只建窗口、不带任何会话状态：新窗口是独立 webview，有自己的 Pinia，挂载后自己
/// 从 label 里的 `HostId` 去开远端栏。这里刻意不做「顺手把面板也开好」——那会让
/// 建窗口与拨号耦合，而拨号失败时用户至少还能看到本机栏。
///
/// 去边框（标题栏由 `FileManager.vue` 自绘）配 `shadow(true)` 显式写出来：这个组合
/// 在 Windows 上是 tao 的「with_undecorated_shadow」模式，DWM 阴影与左右/下边缘的
/// 原生缩放都保留，只把顶部那条留给 tauri-runtime-wry 的 resize helper。
///
/// macOS 是另一套：原生 Overlay 标题栏 + 红绿灯，绿灯那颗就是全屏。见
/// `with_platform_chrome` 与 `tauri.macos.conf.json`（主窗口在配置里，这里是运行期建的窗口）。
pub fn open_file_window_impl(app: &AppHandle, host_id: HostId) -> Result<(), IpcError> {
    let label = format!("{FILE_WINDOW_PREFIX}{}", host_id.0);
    if let Some(window) = app.get_webview_window(&label) {
        // 同一台主机只留一个文件窗口；已存在就提到前面，而不是再开一个抢同一个连接。
        let _ = window.set_focus();
        return Ok(());
    }
    let builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title("dbhx 文件管理")
        .inner_size(1100.0, 700.0)
        .min_inner_size(760.0, 460.0)
        .resizable(true)
        .shadow(true);
    with_platform_chrome(builder)
        .build()
        .map_err(|e| {
            tracing::debug!(%e, "创建文件管理窗口失败");
            IpcError::new(ErrorCode::Internal, "无法打开文件管理窗口", false)
        })?;
    Ok(())
}

/// 窗口边框的平台差异。拆成两个 `#[cfg]` 函数而不是 `if cfg!(...)`：后者的两个分支
/// 都要过编译，而 `title_bar_style` / `hidden_title` / `traffic_light_position` 在
/// tao 上是 `#[cfg(target_os = "macos")]` 方法，在 Windows 上直接不存在。
#[cfg(target_os = "macos")]
fn with_platform_chrome<R: tauri::Runtime, M: Manager<R>>(
    builder: WebviewWindowBuilder<'_, R, M>,
) -> WebviewWindowBuilder<'_, R, M> {
    builder
        .decorations(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
    // 不设 traffic_light_position：macOS 26 实测 tao 的 inset 不生效（改 y 灯不动），
    // 灯就停在系统默认位（32pt 标题栏居中）。前端工具栏因此在 mac 上收成 32px 去对齐，
    // 见 `App.vue` 的 `.bar--mac`。
}

#[cfg(not(target_os = "macos"))]
fn with_platform_chrome<R: tauri::Runtime, M: Manager<R>>(
    builder: WebviewWindowBuilder<'_, R, M>,
) -> WebviewWindowBuilder<'_, R, M> {
    builder.decorations(false)
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

// 这条命令壳刻意不写 doc comment（`///`）：它会原样进 bindings.ts 的 JSDoc，而对话框
// 这条前端看一眼签名就懂了。async 的理由写在 `pick_dir_impl` 上。
#[tauri::command]
#[specta::specta]
pub async fn local_pick_dir(default_path: Option<String>) -> Option<String> {
    pick_dir_impl(default_path).await
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

#[tauri::command]
#[specta::specta]
pub async fn local_stat(path: String) -> Result<FileEntry, IpcError> {
    stat_impl(path)
}

#[tauri::command]
#[specta::specta]
pub async fn local_create_file(path: String) -> Result<(), IpcError> {
    create_file_impl(path)
}

#[tauri::command]
#[specta::specta]
pub async fn local_edit_dir() -> Result<String, IpcError> {
    edit_dir_impl()
}

/// 同样是**必须 `async`**：这条会把 spawn 出去的进程交给阻塞线程池，事件循环线程上
/// 不该出现任何「等外部程序」的动作。
#[tauri::command]
#[specta::specta]
pub async fn local_open(path: String) -> Result<(), IpcError> {
    open_with_impl(path)
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

    /// 新建空文件最该守住的一条：同名时**报错**，不是静默截断。
    #[test]
    fn create_file_refuses_to_truncate_an_existing_one() {
        let dir = tempdir("newfile");
        let file = dir.join("note.txt");

        create_file_impl(file.display().to_string()).unwrap();
        assert!(file.is_file(), "建出来的要真是普通文件");
        std::fs::write(&file, b"keep me").unwrap();

        let err = create_file_impl(file.display().to_string()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert_eq!(err.message, "该名称已存在");
        assert_eq!(std::fs::read(&file).unwrap(), b"keep me", "已存在的文件一个字节都不能少");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stat_reads_one_entry_back_without_the_whole_directory() {
        let dir = tempdir("stat");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"12345").unwrap();

        let entry = stat_impl(file.display().to_string()).unwrap();
        assert_eq!((entry.name.as_str(), entry.kind, entry.size), ("a.txt", FileKind::File, 5));
        assert!(entry.mtime_ms.is_some(), "本机文件该带修改时间，编辑流程就靠它比对");

        let err = stat_impl(dir.join("gone.txt").display().to_string()).unwrap_err();
        assert_eq!(err.message, "路径不存在", "OS 的英文说明不外泄");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_dir_lives_under_the_system_temp() {
        let dir = edit_dir_impl().unwrap();
        assert!(std::fs::metadata(&dir).is_ok_and(|m| m.is_dir()), "外部编辑的临时目录要真的存在：{dir}");
        assert!(dir.replace('\\', "/").ends_with("dbhx-edit"), "临时副本集中放一处，好认也好清：{dir}");
    }

    /// 关窗会把前端 `finally` 的清理一起带走，所以过期副本要由后端扫掉——
    /// 但只扫过期的，正在编辑的那份碰不得。
    #[test]
    fn prune_removes_only_past_the_deadline() {
        let dir = tempdir("prune");
        let fresh = dir.join("editing.txt");
        let stale = dir.join("leftover.txt");
        std::fs::write(&fresh, b"new").unwrap();
        std::fs::write(&stale, b"old").unwrap();

        // 期限设成 0：两个都算「过期」，全清
        prune_stale_edits(&dir, std::time::Duration::ZERO);
        assert!(!fresh.exists() && !stale.exists(), "过期的编辑副本一个都不该留");

        // 期限放开一小时：刚写出来的还年轻，动它就会毁掉用户正在编辑的文件
        std::fs::write(&fresh, b"new").unwrap();
        prune_stale_edits(&dir, EDIT_STALE_AFTER);
        assert!(fresh.exists(), "正在编辑的副本不能被误删");

        // 目录本身删不掉（remove_file 对目录报错），但清理不该因此把正事挡下
        std::fs::create_dir(dir.join("sub")).unwrap();
        prune_stale_edits(&dir, std::time::Duration::ZERO);
        assert!(dir.join("sub").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_on_a_missing_directory_is_silent() {
        // 第一次用这个应用时目录还没建过，打扫必须是空操作而不是 panic 或报错
        prune_stale_edits(Path::new("Z:\\dbhx-no-such-dir"), std::time::Duration::ZERO);
    }
}
