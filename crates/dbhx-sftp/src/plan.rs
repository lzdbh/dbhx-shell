//! 批量传输的任务表：把「一个目录」摊平成「一串文件任务」。
//!
//! 远端列举走 [`Lister`] 这个窄口子（与 `dbhx-session` 的 `Dialer` 同一个套路），
//! 因此目录树展开、符号链接不跟随、深度上限这些规则都能在内存里测，不需要真连接。
//! 本机侧用 `std::fs`，它本来就是本地的。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use dbhx_model::{FileEntry, FileKind, TransferKind};

use crate::error::SftpError;
use crate::path;

/// 手写 boxed future：不引 `futures`，类型别名里也就不用写 async fn。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 远端目录列举。参数是绝对路径（已归一化），返回该目录下的直接子项。
pub type Lister =
    Box<dyn Fn(String) -> BoxFuture<'static, Result<Vec<FileEntry>, SftpError>> + Send + Sync>;

/// 一条文件任务。`size` 取自规划时的元数据，仅用于进度显示——
/// 真正的字节数以传输过程为准，服务端在传输期间改文件是常态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub remote: String,
    pub local: PathBuf,
    pub size: u64,
}

/// 一批任务，加上「先要在哪一侧建出哪些目录」。
///
/// 目录单列出来是因为空目录也要保留：只摊文件的话，传一个空目录会安静地什么也不做。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub kind: TransferKind,
    pub tasks: Vec<Task>,
    /// 下载：本机侧要先建出来的目录
    pub local_dirs: Vec<PathBuf>,
    /// 上传：远端侧要先建出来的目录
    pub remote_dirs: Vec<String>,
    pub total_bytes: u64,
}

impl Plan {
    fn new(kind: TransferKind) -> Self {
        Self { kind, tasks: Vec::new(), local_dirs: Vec::new(), remote_dirs: Vec::new(), total_bytes: 0 }
    }

    fn push_file(&mut self, task: Task) {
        self.total_bytes += task.size;
        self.tasks.push(task);
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.local_dirs.is_empty() && self.remote_dirs.is_empty()
    }

    /// 目录任务的顺序决定进度条观感：先短后长会把「还剩 3 个」一直挂在最后。
    /// 这里按体积从大到小排，先吃大文件，剩下的都是快的。
    pub fn sort_by_size_desc(&mut self) {
        self.tasks.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.remote.cmp(&b.remote)));
    }
}

/// 目录树展开的上限。
///
/// 不是性能考虑，是防死循环：`/` 下有人写了 `a -> .` 这种链接时，
/// 不跟随链接就已经够了，深度上限再兜一层防止别的形式的回环。
pub const MAX_DEPTH: usize = 32;

/// 摊平一个远端目录（下载）。
///
/// 符号链接**一律不跟随**：跟了就会把 `/` 下的自链接环搬空，而用户的预期是
/// 「把日志目录搬下来」，链接指向的那份多半已经在别处。
pub async fn expand_remote(
    list: &Lister,
    remote_root: &str,
    local_root: &Path,
) -> Result<Plan, SftpError> {
    let mut plan = Plan::new(TransferKind::Download);
    // 根目录本身要建：用户选「下载 logs」时期待得到 local_root/logs
    let local_self = local_dir_for(remote_root, local_root);
    plan.local_dirs.push(local_self.clone());
    walk_remote(list, remote_root, &local_self, 0, &mut plan).await?;
    Ok(plan)
}

fn local_dir_for(remote: &str, local_root: &Path) -> PathBuf {
    let name = path::basename(remote).unwrap_or("root");
    local_root.join(name)
}

/// 摊平一批远端选择（下载）。
///
/// 每一项的类型只能从**父目录的列举**里读：[`Lister`] 走 readdir，条目带的是 lstat
/// 的 mode 位，所以符号链接不会被误当成目录展开（改用 `opendir` 试探的话会，
/// SFTP 服务端在 opendir 时是跟随链接的）。父目录正是面板当前展示的那一层——
/// 能勾到的项本来就在同一层，所以同一层的多项勾选只列举一次。
///
/// 目录走 [`expand_remote`]，落点是 `local_root/<目录名>`；
/// 文件直接落 `local_root/<文件名>`——两种混选各自独立，互不影响。
pub async fn expand_remote_paths(
    list: &Lister,
    remote_paths: &[String],
    local_root: &Path,
) -> Result<Plan, SftpError> {
    let mut plan = Plan::new(TransferKind::Download);
    let mut parents: HashMap<String, Vec<FileEntry>> = HashMap::new();
    for raw in remote_paths {
        let remote = path::normalize(raw);
        // 根目录（没有父）直接算「不存在」：下载 `/` 本来就不是一个合理操作
        let parent = path::parent(&remote).ok_or_else(|| SftpError::BadPath(remote.clone()))?;
        if !parents.contains_key(&parent) {
            let entries = list(parent.clone()).await?;
            parents.insert(parent.clone(), entries);
        }
        let entry = parents
            .get(&parent)
            .and_then(|entries| entries.iter().find(|e| e.path == remote))
            .ok_or_else(|| SftpError::NotFound(remote.clone()))?
            .clone();
        if entry.kind == FileKind::Dir {
            merge(&mut plan, expand_remote(list, &remote, local_root).await?);
            continue;
        }
        if entry.kind == FileKind::Symlink || entry.kind == FileKind::Other {
            // 与目录遍历同规则：链接和特殊文件搬过去也读不出内容
            tracing::debug!(path = %remote, "非普通文件，跳过");
            continue;
        }
        let name = path::basename(&remote).ok_or_else(|| SftpError::BadPath(remote.clone()))?;
        plan.push_file(Task { remote: remote.clone(), local: local_root.join(name), size: entry.size });
    }
    Ok(plan)
}

/// 合并两个计划。目录条目要去重：勾选 `logs` 与 `logs/archive` 时，
/// 同一个本地目录会被两条各自建一次。
fn merge(into: &mut Plan, other: Plan) {
    for dir in other.local_dirs {
        if !into.local_dirs.contains(&dir) {
            into.local_dirs.push(dir);
        }
    }
    for remote_dir in other.remote_dirs {
        if !into.remote_dirs.contains(&remote_dir) {
            into.remote_dirs.push(remote_dir);
        }
    }
    into.tasks.extend(other.tasks);
    into.total_bytes += other.total_bytes;
}

/// 递归列举。签名手写 `BoxFuture` 而不是 `async fn`：
/// 异步函数自递归必须装箱，`Box::pin` 包住调用点并不能绕过 E0733。
fn walk_remote<'a>(
    list: &'a Lister,
    remote_dir: &'a str,
    local_dir: &'a Path,
    depth: usize,
    plan: &'a mut Plan,
) -> BoxFuture<'a, Result<(), SftpError>> {
    Box::pin(async move {
        if depth > MAX_DEPTH {
            // 静默跳过而不是报错：一个 32 层深的目录树多半是某个缓存，用户不想知道它存在
            tracing::debug!(dir = %remote_dir, "目录层级过深，跳过");
            return Ok(());
        }
        for entry in list(remote_dir.to_string()).await? {
            match entry.kind {
                FileKind::Dir => {
                    let child_local = local_dir.join(&entry.name);
                    plan.local_dirs.push(child_local.clone());
                    walk_remote(list, &entry.path, &child_local, depth + 1, plan).await?;
                }
                FileKind::File => {
                    plan.push_file(Task {
                        remote: entry.path.clone(),
                        local: local_dir.join(&entry.name),
                        size: entry.size,
                    });
                }
                // 链接与特殊文件（FIFO、设备）不搬：SFTP 上它们既读不出内容也建不出来
                FileKind::Symlink | FileKind::Other => {
                    tracing::debug!(entry = %entry.path, "非普通文件，跳过");
                }
            }
        }
        Ok(())
    })
}

/// 摊平本机的若干路径（上传）。
///
/// 与下载同规则：符号链接不跟随，特殊文件跳过。参数里既可以混文件也可以混目录，
/// 面板「拖一批进来」就是这种形状。
pub fn expand_local(local: &[PathBuf], remote_dir: &str) -> Result<Plan, SftpError> {
    let mut plan = Plan::new(TransferKind::Upload);
    for src in local {
        let meta = std::fs::symlink_metadata(src).map_err(|e| SftpError::local_io(&src.display().to_string(), &e))?;
        if meta.is_symlink() {
            tracing::debug!(path = %src.display(), "符号链接，跳过");
            continue;
        }
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| SftpError::BadPath(src.display().to_string()))?;
        let remote = path::join(remote_dir, &name);
        if meta.is_dir() {
            plan.remote_dirs.push(remote.clone());
            walk_local(src, &remote, 0, &mut plan)?;
        } else if meta.is_file() {
            plan.push_file(Task { remote, local: src.clone(), size: meta.len() });
        }
    }
    Ok(plan)
}

fn walk_local(local_dir: &Path, remote_dir: &str, depth: usize, plan: &mut Plan) -> Result<(), SftpError> {
    if depth > MAX_DEPTH {
        tracing::debug!(dir = %local_dir.display(), "目录层级过深，跳过");
        return Ok(());
    }
    let read = std::fs::read_dir(local_dir).map_err(|e| SftpError::local_io(&local_dir.display().to_string(), &e))?;
    for item in read {
        let item = item.map_err(|e| SftpError::local_io(&local_dir.display().to_string(), &e))?;
        let meta = std::fs::symlink_metadata(item.path())
            .map_err(|e| SftpError::local_io(&item.path().display().to_string(), &e))?;
        let name = item.file_name().to_string_lossy().to_string();
        let remote = path::join(remote_dir, &name);
        if meta.is_symlink() {
            tracing::debug!(path = %item.path().display(), "符号链接，跳过");
            continue;
        }
        if meta.is_dir() {
            plan.remote_dirs.push(remote.clone());
            walk_local(&item.path(), &remote, depth + 1, plan)?;
        } else if meta.is_file() {
            plan.push_file(Task { remote, local: item.path(), size: meta.len() });
        } else {
            // FIFO、设备之类在本地文件系统上读不出内容，搬过去也只会得到半个文件
            tracing::debug!(path = %item.path().display(), "特殊文件，跳过");
        }
    }
    Ok(())
}

/// 用内存里的一棵树造 `Lister`，测试专用。
#[cfg(test)]
pub fn fake_tree(tree: HashMap<String, Vec<FileEntry>>) -> Lister {
    Box::new(move |dir| {
        let tree = tree.clone();
        Box::pin(async move {
            tree.get(&dir).cloned().ok_or_else(|| SftpError::NotFound(dir.clone()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str, parent: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: path::join(parent, name),
            kind: FileKind::Dir,
            size: 4096,
            mtime_ms: None,
            mode: Some(0o755),
            owner: None,
            group: None,
        }
    }

    fn file(name: &str, parent: &str, size: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: path::join(parent, name),
            kind: FileKind::File,
            size,
            mtime_ms: None,
            mode: Some(0o644),
            owner: None,
            group: None,
        }
    }

    fn symlink(name: &str, parent: &str) -> FileEntry {
        FileEntry { kind: FileKind::Symlink, ..file(name, parent, 11) }
    }

    fn tree() -> HashMap<String, Vec<FileEntry>> {
        let mut t = HashMap::new();
        // 勾选根下的第一层时，父目录就是 `/`：混选要先靠它读出「logs 是目录」
        t.insert("/".to_string(), vec![dir("logs", "/")]);
        t.insert(
            "/logs".to_string(),
            vec![
                file("app.log", "/logs", 100),
                dir("archive", "/logs"),
                symlink("latest", "/logs"),
                // 与参考实现里那台机器一样：日志目录下躺着一个 FIFO
                FileEntry { kind: FileKind::Other, ..file("x", "/logs", 0) },
            ],
        );
        t.insert("/logs/archive".to_string(), vec![file("2025.log.gz", "/logs/archive", 50), dir("empty", "/logs/archive")]);
        t.insert("/logs/archive/empty".to_string(), vec![]);
        t
    }

    #[tokio::test]
    async fn download_plan_flattens_directories_and_keeps_empties() {
        let plan = expand_remote(&fake_tree(tree()), "/logs", Path::new("/tmp/dl")).await.unwrap();
        let mut paths: Vec<&str> = plan.tasks.iter().map(|t| t.remote.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["/logs/app.log", "/logs/archive/2025.log.gz"], "符号链接与特殊文件不搬");
        assert_eq!(
            plan.tasks.iter().find(|t| t.remote == "/logs/app.log").unwrap().local,
            PathBuf::from("/tmp/dl/logs/app.log")
        );
        // 空目录必须出现在 local_dirs 里，否则下载一个空目录会静默无操作
        assert!(plan.local_dirs.contains(&PathBuf::from("/tmp/dl/logs/archive/empty")));
        assert_eq!(plan.total_bytes, 150);
        assert!(plan.remote_dirs.is_empty());
    }

    #[tokio::test]
    async fn missing_root_directory_is_an_error_not_an_empty_plan() {
        let err = expand_remote(&fake_tree(tree()), "/nope", Path::new("/tmp/dl")).await.unwrap_err();
        assert!(matches!(err, SftpError::NotFound(p) if p == "/nope"));
    }

    #[tokio::test]
    async fn a_mixed_selection_expands_dirs_and_keeps_loose_files() {
        // 面板里勾「一个目录 + 一个文件」是最常见的操作，两种落点规则不一样
        let list = fake_tree(tree());
        let plan = expand_remote_paths(&list, &["/logs".to_string(), "/logs/app.log".to_string()], Path::new("/tmp/dl"))
            .await
            .unwrap();
        let mut remotes: Vec<&str> = plan.tasks.iter().map(|t| t.remote.as_str()).collect();
        remotes.sort_unstable();
        assert_eq!(remotes, ["/logs/app.log", "/logs/app.log", "/logs/archive/2025.log.gz"], "混选按勾选逐项展开");

        let loose = plan.tasks.iter().find(|t| t.local == PathBuf::from("/tmp/dl/app.log")).unwrap();
        assert_eq!(loose.size, 100, "文件大小取自规划时的元数据");
        assert!(plan.local_dirs.contains(&PathBuf::from("/tmp/dl/logs")), "目录项仍按 logs/ 落");
        assert_eq!(plan.total_bytes, 100 + 100 + 50);
    }

    #[tokio::test]
    async fn a_selected_symlink_is_skipped_rather_than_read() {
        let list = fake_tree(tree());
        let plan = expand_remote_paths(&list, &["/logs/latest".to_string()], Path::new("/tmp/dl")).await.unwrap();
        assert!(plan.is_empty(), "勾选单个符号链接不该搬任何东西，也不该报错");
    }

    #[tokio::test]
    async fn a_vanished_selection_reports_not_found() {
        // 面板停留期间远端把文件删了：要报「不存在」，而不是静默交付一个空计划
        let list = fake_tree(tree());
        let err = expand_remote_paths(&list, &["/logs/gone.log".to_string()], Path::new("/tmp/dl")).await.unwrap_err();
        assert!(matches!(err, SftpError::NotFound(p) if p == "/logs/gone.log"));
    }

    #[tokio::test]
    async fn runaway_depth_is_cut_off() {
        // 一条 40 层深的链，每层只有一个子目录，最底层放一个文件
        let mut t = HashMap::new();
        let mut prefix = "/deep".to_string();
        for i in 0..40 {
            let name = i.to_string();
            let child = path::join(&prefix, &name);
            t.insert(prefix.clone(), vec![dir(&name, &prefix)]);
            prefix = child;
        }
        t.insert(prefix.clone(), vec![file("bottom", &prefix, 1)]);
        let plan = expand_remote(&fake_tree(t), "/deep", Path::new("/tmp/dl")).await.unwrap();
        assert!(plan.tasks.is_empty(), "超过 {MAX_DEPTH} 层不再往下走");
        assert!(!plan.local_dirs.is_empty(), "浅层的目录仍然建了");
    }

    #[test]
    fn upload_plan_walks_the_local_filesystem() {
        let root = std::env::temp_dir().join(format!("dbhx-sftp-plan-{}", std::process::id()));
        let nested = root.join("up").join("conf");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("app.yaml"), "a: 1").unwrap();
        std::fs::write(root.join("up").join("big.bin"), vec![7u8; 12]).unwrap();

        let plan = expand_local(&[root.join("up")], "/srv").unwrap();
        let mut names: Vec<&str> = plan.tasks.iter().map(|t| t.remote.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["/srv/up/big.bin", "/srv/up/conf/app.yaml"]);
        assert_eq!(plan.total_bytes, 12 + 4);
        assert!(plan.remote_dirs.contains(&"/srv/up/conf".to_string()), "空目录也要在远端建出来");
        assert!(plan.local_dirs.is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn upload_of_a_missing_local_path_reports_the_path() {
        let missing = std::env::temp_dir().join("dbhx-sftp-definitely-absent");
        let err = expand_local(&[missing.clone()], "/srv").unwrap_err();
        assert!(matches!(err, SftpError::LocalIo { .. }), "{err:?}");
        assert!(format!("{err}").contains(&missing.display().to_string()));
    }

    #[test]
    fn biggest_files_go_first() {
        let mut plan = Plan {
            kind: TransferKind::Download,
            tasks: vec![
                Task { remote: "/a/small".into(), local: "/x/small".into(), size: 1 },
                Task { remote: "/a/huge".into(), local: "/x/huge".into(), size: 900 },
                Task { remote: "/a/mid".into(), local: "/x/mid".into(), size: 900 },
            ],
            local_dirs: Vec::new(),
            remote_dirs: Vec::new(),
            total_bytes: 1801,
        };
        plan.sort_by_size_desc();
        let names: Vec<&str> = plan.tasks.iter().map(|t| t.remote.as_str()).collect();
        // 同体积时按路径定序，进度列表才不会每次刷新都换行
        assert_eq!(names, ["/a/huge", "/a/mid", "/a/small"]);
    }
}
