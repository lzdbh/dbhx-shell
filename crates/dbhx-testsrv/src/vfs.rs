//! 内存虚拟文件系统：测试服务器背后的「远端磁盘」。
//!
//! 为什么不用真目录：
//! * 被测代码的一条硬规矩是**绝不跟随符号链接**，而 F: 卷是 exFAT，根本建不出符号链接。
//!   只有自研的节点表能造出「目录项里躺着 symlink」这个必须覆盖的形态。
//! * 真目录要清理、会互相干扰，并行跑测试时路径也无法预知。
//!
//! 语义上刻意贴近 OpenSSH 的 SFTP 服务端：路径一律 POSIX 风格、`.`/`..` 在解析时折叠、
//! 符号链接的 target 可以是相对写法（相对**链接所在目录**，不是进程 cwd）。
//!
//! 所有查询返回拥有的快照（[`Stat`]）而不是引用：这是一份测试夹具，
//! 数据集小到可以忽略拷贝成本，换来的是调用方不用跟借用检查器缠斗。

use std::collections::BTreeMap;

/// 节点类型。只有这三种，够覆盖面板与传输引擎会碰到的所有形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    data: Vec<u8>,
    /// `Kind::Symlink` 的目标原文（未解析）
    target: String,
    mode: u32,
    mtime: u32,
}

/// 一次 `stat`/`lstat` 的全部可见信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// 解析后的绝对路径。`follow=false` 时就是请求路径本身。
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime: u32,
    /// 仅符号链接有值
    pub target: Option<String>,
}

/// VFS 层能区分的失败原因。协议层怎么把它翻译成 `SSH_FX_*` 是 sftp.rs 的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    /// 要目录的地方是文件
    NotDir,
    /// 要文件的地方是目录
    IsDir,
    /// 路径本身不合法
    BadPath,
    /// 符号链接绕圈
    Loop,
}

/// 符号链接最多折这么多次，再多判定为环。
const MAX_SYMLINK_HOPS: usize = 8;

/// [`Vfs::walk_once`] 的三种结局。
enum Walk {
    /// 全程没有需要解引用的链接，这就是终点
    Done(String),
    /// 中途撞上链接，换成目标后应当再走一遍
    Redirect(String),
    /// 某一段不存在
    Missing,
}

fn split(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty() && *s != ".").collect()
}

fn assemble(parts: &[&str]) -> String {
    if parts.is_empty() { "/".to_string() } else { format!("/{}", parts.join("/")) }
}

/// 把 `raw` 按 `base` 展开并折叠 `..`。`raw` 以 `/` 开头时 `base` 被忽略。
fn join_abs(base: &str, raw: &str) -> Result<String, FsError> {
    if raw.contains('\0') {
        return Err(FsError::BadPath);
    }
    let mut parts: Vec<&str> = if raw.starts_with('/') { Vec::new() } else { split(base) };
    for seg in split(raw) {
        if seg == ".." {
            parts.pop();
        } else {
            parts.push(seg);
        }
    }
    Ok(assemble(&parts))
}

fn parent_of(path: &str) -> Option<&str> {
    if path == "/" {
        return None;
    }
    let idx = path.rfind('/')?;
    Some(if idx == 0 { "/" } else { &path[..idx] })
}

#[derive(Debug, Default)]
pub struct Vfs {
    /// key 恒为规范化后的绝对路径。BTreeMap 的字典序正好是目录列举的稳定顺序。
    nodes: BTreeMap<String, Node>,
    clock: u32,
}

impl Vfs {
    /// 建一个只有根目录的文件系统。
    pub fn new() -> Self {
        let mut this = Self::default();
        this.insert("/", Kind::Dir, Vec::new(), "", 0o755);
        this
    }

    fn tick(&mut self) -> u32 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn insert(&mut self, path: &str, kind: Kind, data: Vec<u8>, target: &str, mode: u32) {
        let mtime = self.tick();
        self.nodes.insert(path.to_string(), Node { kind, data, target: target.to_string(), mode, mtime });
    }

    // ---- 夹具侧的写接口（不走协议，返回 Self 以便链式搭场景）----

    /// 建目录。父级必须已经存在——不像 `mkdir -p`，测试应当显式表达层级。
    pub fn add_dir(&mut self, path: &str) -> &mut Self {
        self.insert(&normalize(path), Kind::Dir, Vec::new(), "", 0o755);
        self
    }

    pub fn add_file(&mut self, path: &str, bytes: impl AsRef<[u8]>) -> &mut Self {
        self.insert(&normalize(path), Kind::File, bytes.as_ref().to_vec(), "", 0o644);
        self
    }

    pub fn add_symlink(&mut self, link: &str, target: &str) -> &mut Self {
        self.insert(&normalize(link), Kind::Symlink, Vec::new(), target, 0o777);
        self
    }

    /// 断言用：读内容（跟随符号链接）。
    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let final_path = self.resolve_link(&normalize(path), true)?;
        self.nodes.get(&final_path).filter(|n| n.kind == Kind::File).map(|n| n.data.clone())
    }

    pub fn exists(&self, path: &str) -> bool {
        self.stat(&normalize(path), false).is_ok()
    }

    pub fn list_paths(&self, dir: &str) -> Vec<String> {
        self.list(dir).into_iter().map(|(p, _)| p).collect()
    }

    // ---- 协议侧 ----

    /// 把客户端给的写法折成绝对路径。相对写法按 `cwd` 展开。
    pub fn resolve(&self, raw: &str, cwd: &str) -> Result<String, FsError> {
        if raw.is_empty() || raw.contains('\0') {
            return Err(FsError::BadPath);
        }
        join_abs(cwd, raw)
    }

    /// 沿路径逐段解符号链接，返回终点路径。
    ///
    /// `follow=false` 时只解**中间**各段（和 `lstat` 一致：末段是链接就返回链接本身），
    /// `follow=true` 时末段也解（和 `stat`、`realpath` 一致）。
    ///
    /// 一次走到底太绕（链接目标本身可能又是链接），改成「走一遍，遇到需要解引用的
    /// 就地替换，再从头走」的定次循环，`MAX_SYMLINK_HOPS` 同时充当环检测的预算。
    pub fn resolve_link(&self, path: &str, follow_final: bool) -> Option<String> {
        let mut work = normalize(path);
        let mut hops = 0usize;
        loop {
            match self.walk_once(&work, follow_final) {
                Walk::Done(resolved) => return Some(resolved),
                Walk::Missing => return None,
                Walk::Redirect(next) => {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return None;
                    }
                    work = normalize(&next);
                }
            }
        }
    }

    /// 走一遍路径，把第一个需要解引用的符号链接换成它的目标。
    fn walk_once(&self, path: &str, follow_final: bool) -> Walk {
        let mut current = String::new();
        let segments = split(path);
        for (index, seg) in segments.iter().enumerate() {
            let candidate = format!("{current}/{seg}");
            let Some(node) = self.nodes.get(&candidate) else { return Walk::Missing };
            let is_last = index + 1 == segments.len();
            if node.kind == Kind::Symlink && (follow_final || !is_last) {
                let base = if current.is_empty() { "/" } else { current.as_str() };
                let target = match join_abs(base, &node.target) {
                    Ok(t) => t,
                    Err(_) => return Walk::Missing,
                };
                let rest = segments[index + 1..].join("/");
                return Walk::Redirect(if rest.is_empty() { target } else { format!("{target}/{rest}") });
            }
            current = candidate;
        }
        Walk::Done(if current.is_empty() { "/".to_string() } else { current })
    }

    /// `STAT` / `LSTAT`。返回的 `path` 是**解析过中间段**的形态，
    /// 这正是协议里 `lstat` 该报的路径（末段链接不解引用，但中间段要解）。
    pub fn stat(&self, path: &str, follow: bool) -> Result<Stat, FsError> {
        let final_path = self.resolve_link(path, follow).ok_or(FsError::NotFound)?;
        let node = self.nodes.get(&final_path).ok_or(FsError::NotFound)?;
        Ok(Stat {
            path: final_path,
            kind: node.kind,
            size: node.data.len() as u64,
            mode: node.mode,
            mtime: node.mtime,
            target: (node.kind == Kind::Symlink).then(|| node.target.clone()),
        })
    }

    pub fn read_link(&self, path: &str) -> Result<String, FsError> {
        let node = self.nodes.get(path).ok_or(FsError::NotFound)?;
        if node.kind != Kind::Symlink {
            return Err(FsError::NotFound);
        }
        Ok(node.target.clone())
    }

    /// 目录的直接子项，按名字典序。返回 `(完整路径, 快照)`。
    pub fn list(&self, dir: &str) -> Vec<(String, Stat)> {
        let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
        self.nodes
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix) && **k != dir)
            .filter(|(k, _)| k[prefix.len()..].find('/').is_none())
            .map(|(k, v)| (k.clone(), Stat { path: k.clone(), kind: v.kind, size: v.data.len() as u64, mode: v.mode, mtime: v.mtime, target: (v.kind == Kind::Symlink).then(|| v.target.clone()) }))
            .collect()
    }

    /// 打开文件（`SSH_FXP_OPEN`）。链接先解引用，和真实服务端一致。
    pub fn open(&mut self, path: &str, create: bool, truncate: bool) -> Result<String, FsError> {
        let final_path = match self.resolve_link(path, true) {
            Some(p) => p,
            None => {
                if !create {
                    return Err(FsError::NotFound);
                }
                if parent_of(path).is_some_and(|p| !self.is_dir(p)) {
                    return Err(FsError::NotFound);
                }
                self.insert(path, Kind::File, Vec::new(), "", 0o644);
                return Ok(path.to_string());
            }
        };
        let kind = self.nodes.get(&final_path).ok_or(FsError::NotFound)?.kind;
        if kind == Kind::Dir {
            return Err(FsError::IsDir);
        }
        if truncate {
            self.tick();
            self.nodes.get_mut(&final_path).unwrap().data.clear();
        }
        Ok(final_path)
    }

    pub fn write_at(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), FsError> {
        let start = offset as usize;
        let mtime = self.tick();
        let node = self.nodes.get_mut(path).ok_or(FsError::NotFound)?;
        if node.kind == Kind::Dir {
            return Err(FsError::IsDir);
        }
        let end = start + bytes.len();
        if node.data.len() < end {
            node.data.resize(end, 0);
        }
        node.data[start..end].copy_from_slice(bytes);
        node.mtime = mtime;
        Ok(())
    }

    /// 读文件。越过末尾返回空切片——`SSH_FXP_READ` 到文件尾由协议层翻译成 EOF。
    pub fn read_at(&self, path: &str, offset: u64, len: u32) -> Result<Vec<u8>, FsError> {
        let node = self.nodes.get(path).ok_or(FsError::NotFound)?;
        if node.kind == Kind::Dir {
            return Err(FsError::IsDir);
        }
        let start = (offset as usize).min(node.data.len());
        let end = start.saturating_add(len as usize).min(node.data.len());
        Ok(node.data[start..end].to_vec())
    }

    pub fn mkdir(&mut self, path: &str) -> Result<(), FsError> {
        if self.nodes.contains_key(path) {
            return Err(FsError::Exists);
        }
        if parent_of(path).is_some_and(|p| !self.is_dir(p)) {
            return Err(FsError::NotFound);
        }
        let mtime = self.tick();
        self.nodes.insert(path.to_string(), Node { kind: Kind::Dir, data: Vec::new(), target: String::new(), mode: 0o755, mtime });
        Ok(())
    }

    pub fn opendir(&self, path: &str) -> Result<(), FsError> {
        match self.resolve_link(path, true).and_then(|p| self.nodes.get(&p).map(|n| n.kind)) {
            Some(Kind::Dir) => Ok(()),
            Some(Kind::File) => Err(FsError::NotDir),
            Some(Kind::Symlink) => Err(FsError::NotFound),
            None => Err(FsError::NotFound),
        }
    }

    /// 删除**文件**（`SSH_FXP_REMOVE`）。目录要走 `remove_dir`。
    pub fn remove_file(&mut self, path: &str) -> Result<(), FsError> {
        let Some(node) = self.nodes.get(path).map(|n| n.kind) else {
            return Err(FsError::NotFound);
        };
        if node == Kind::Dir {
            return Err(FsError::IsDir);
        }
        self.nodes.remove(path);
        Ok(())
    }

    pub fn remove_dir(&mut self, path: &str) -> Result<(), FsError> {
        match self.nodes.get(path).map(|n| n.kind) {
            None => return Err(FsError::NotFound),
            Some(Kind::File) => return Err(FsError::NotDir),
            Some(Kind::Symlink) => return Err(FsError::NotDir),
            Some(Kind::Dir) => {}
        }
        if !self.list(path).is_empty() {
            return Err(FsError::Exists);
        }
        self.nodes.remove(path);
        Ok(())
    }

    /// 纯改名。是否覆盖由调用方判断——SFTP v3 的 `RENAME` 本身没有覆盖语义。
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        if !self.nodes.contains_key(from) {
            return Err(FsError::NotFound);
        }
        if to == from {
            return Ok(());
        }
        if self.nodes.contains_key(to) {
            return Err(FsError::Exists);
        }
        if parent_of(to).is_some_and(|p| !self.is_dir(p)) {
            return Err(FsError::NotFound);
        }
        let mut node = self.nodes.remove(from).ok_or(FsError::NotFound)?;
        node.mtime = self.tick();
        self.nodes.insert(to.to_string(), node);
        Ok(())
    }

    pub fn symlink(&mut self, link: &str, target: &str) -> Result<(), FsError> {
        if self.nodes.contains_key(link) {
            return Err(FsError::Exists);
        }
        if parent_of(link).is_some_and(|p| !self.is_dir(p)) {
            return Err(FsError::NotFound);
        }
        let mtime = self.tick();
        self.nodes.insert(link.to_string(), Node { kind: Kind::Symlink, data: Vec::new(), target: target.to_string(), mode: 0o777, mtime });
        Ok(())
    }

    /// `SSH_FXP_SETSTAT` 里带 size 时的截断。其余属性本夹具不建模。
    pub fn set_size(&mut self, path: &str, size: u64) -> Result<(), FsError> {
        let want = size as usize;
        let mtime = self.tick();
        let node = self.nodes.get_mut(path).ok_or(FsError::NotFound)?;
        if node.kind == Kind::Dir {
            return Err(FsError::IsDir);
        }
        node.data.resize(want, 0);
        node.mtime = mtime;
        Ok(())
    }

    pub fn is_dir(&self, path: &str) -> bool {
        self.nodes.get(path).is_some_and(|n| n.kind == Kind::Dir)
    }
}

fn basename_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

/// 折叠 `.`/`..` 并保证以 `/` 开头。给夹具用的宽松版本。
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in split(path) {
        if p == ".." {
            parts.pop();
        } else {
            parts.push(p);
        }
    }
    assemble(&parts)
}

pub fn parent(path: &str) -> Option<String> {
    parent_of(path).map(str::to_string)
}

pub fn basename(path: &str) -> String {
    basename_of(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个够跑完所有分支的场景：目录、文件、绝对链接、相对链接、二重链接、环。
    fn fs() -> Vfs {
        let mut fs = Vfs::new();
        fs.add_dir("/home");
        fs.add_dir("/home/app");
        fs.add_dir("/home/app/logs");
        fs.add_file("/home/app/readme.md", b"hello\n".to_vec());
        fs.add_file("/home/app/logs/app.log", b"log\n".to_vec());
        fs.add_symlink("/home/app/abs", "/home/app/readme.md");
        fs.add_symlink("/home/app/rel", "readme.md");
        fs.add_symlink("/home/app/two", "abs");
        fs.add_symlink("/loop-a", "/loop-b");
        fs.add_symlink("/loop-b", "/loop-a");
        fs
    }

    fn resolved(path: &str) -> Option<String> {
        fs().resolve_link(path, true)
    }

    #[test]
    fn relative_writes_are_expanded_against_the_session_cwd() {
        let fs = fs();
        assert_eq!(fs.resolve("readme.md", "/home/app").unwrap(), "/home/app/readme.md");
        assert_eq!(fs.resolve("./logs/../readme.md", "/home/app").unwrap(), "/home/app/readme.md");
        assert_eq!(fs.resolve("/", "/home/app").unwrap(), "/", "绝对写法不该被 cwd 影响");
        assert_eq!(fs.resolve("..", "/home/app/logs").unwrap(), "/home/app");
        // 根之上再往上仍是根，和 POSIX 一致
        assert_eq!(fs.resolve("../../..", "/home").unwrap(), "/");
    }

    #[test]
    fn links_resolve_to_their_target_and_intermediate_segments_resolve_too() {
        assert_eq!(resolved("/home/app/rel").as_deref(), Some("/home/app/readme.md"));
        assert_eq!(resolved("/home/app/abs").as_deref(), Some("/home/app/readme.md"));
        // 链接的目标又是链接时要继续折到底，否则客户端拿到的是中间态
        assert_eq!(resolved("/home/app/two").as_deref(), Some("/home/app/readme.md"));
        // 中间段是链接：路径后半截要接在解引用后的目标上
        assert_eq!(resolved("/home/app/abs/../logs/app.log").as_deref(), Some("/home/app/logs/app.log"));
    }

    #[test]
    fn lstat_stops_at_the_last_link_but_still_resolves_the_ones_before_it() {
        let fs = fs();
        let link = fs.stat("/home/app/abs", false).unwrap();
        assert_eq!(link.kind, Kind::Symlink);
        assert_eq!(link.path, "/home/app/abs");
        assert_eq!(link.target.as_deref(), Some("/home/app/readme.md"));
        // 同一个位置用 stat 就是文件本体
        assert_eq!(fs.stat("/home/app/abs", true).unwrap().kind, Kind::File);
    }

    #[test]
    fn a_link_loop_is_bounded_instead_of_hanging() {
        // 没有 hops 上限的话这里就是死循环，测试会挂到 CI 超时
        assert!(resolved("/loop-a").is_none());
    }

    #[test]
    fn missing_segments_report_not_found_rather_than_a_partial_path() {
        assert_eq!(fs().stat("/home/app/nope/deeper", true), Err(FsError::NotFound));
        assert_eq!(fs().stat("/home/app/readme.md/x", true), Err(FsError::NotFound));
    }

    #[test]
    fn listing_returns_direct_children_in_dictionary_order() {
        let mut fs = Vfs::new();
        fs.add_dir("/d");
        fs.add_file("/d/b.txt", b"b".to_vec());
        fs.add_file("/d/a.txt", b"a".to_vec());
        fs.add_dir("/d/sub");
        fs.add_file("/d/sub/deep.txt", b"x".to_vec());
        let names: Vec<String> = fs.list("/d").into_iter().map(|(p, _)| p).collect();
        assert_eq!(names, vec!["/d/a.txt", "/d/b.txt", "/d/sub"], "嵌套目录的内容不该混进父级列表");
        assert_eq!(fs.list("/").len(), 1, "根下只有 /d，孙子辈都不该出现");
    }

    #[test]
    fn open_creates_truncates_and_writes_at_offsets() {
        let mut fs = fs();
        assert_eq!(fs.open("/home/app/new.txt", true, false).unwrap(), "/home/app/new.txt");
        fs.write_at("/home/app/new.txt", 0, b"0123456789").unwrap();
        fs.write_at("/home/app/new.txt", 12, b"ab").unwrap();
        assert_eq!(fs.read_at("/home/app/new.txt", 0, 64).unwrap(), b"0123456789\x00\x00ab");
        // 写进链接等于写进它的目标，这是真实服务端的行为。
        // 顺带固定一条容易被想当然的语义：`WRITE` 不截断，只覆盖写入的那段。
        fs.write_at("/home/app/readme.md", 0, b"BYE\n").unwrap();
        assert_eq!(fs.read_file("/home/app/abs").unwrap(), b"BYE\no\n");
        assert_eq!(fs.open("/home/app/new.txt", false, true).unwrap(), "/home/app/new.txt");
        assert!(fs.read_at("/home/app/new.txt", 0, 64).unwrap().is_empty(), "TRUNCATE 必须清空");
        // 打开目录是 IsDir，不是「成功但读不到东西」
        assert_eq!(fs.open("/home/app", false, false), Err(FsError::IsDir));
    }

    #[test]
    fn reading_past_the_end_yields_nothing_and_a_missing_handle_target_is_not_found() {
        let fs = fs();
        assert!(fs.read_at("/home/app/readme.md", 9_000, 32).unwrap().is_empty());
        assert_eq!(fs.read_at("/home/app/missing", 0, 8), Err(FsError::NotFound));
    }

    #[test]
    fn rename_moves_the_node_and_refuses_to_overwrite() {
        let mut fs = fs();
        fs.rename("/home/app/readme.md", "/home/app/moved.md").unwrap();
        assert!(!fs.exists("/home/app/readme.md"));
        assert_eq!(fs.read_file("/home/app/moved.md").unwrap(), b"hello\n");
        assert_eq!(fs.rename("/home/app/moved.md", "/home/app/logs"), Err(FsError::Exists));
        assert_eq!(fs.rename("/home/app/gone.md", "/home/app/x.md"), Err(FsError::NotFound));
    }

    #[test]
    fn directory_removal_requires_an_empty_directory() {
        let mut fs = fs();
        assert_eq!(fs.remove_dir("/home/app"), Err(FsError::Exists), "非空目录不该被删掉");
        assert_eq!(fs.remove_dir("/home/app/readme.md"), Err(FsError::NotDir));
        fs.remove_file("/home/app/readme.md").unwrap();
        fs.remove_dir("/home/app/logs").unwrap_err();
        fs.remove_file("/home/app/logs/app.log").unwrap();
        fs.remove_dir("/home/app/logs").unwrap();
    }

    #[test]
    fn removing_a_file_and_a_symlink_are_both_remove_but_never_a_directory() {
        let mut fs = fs();
        fs.remove_file("/home/app/rel").unwrap();
        assert!(fs.stat("/home/app/rel", false).is_err());
        // 链接还在的目标不该被顺带删掉
        fs.remove_file("/home/app/abs").unwrap();
        assert!(fs.stat("/home/app/readme.md", false).is_ok());
        assert_eq!(fs.remove_file("/home/app/logs"), Err(FsError::IsDir));
    }

    #[test]
    fn creating_inside_a_missing_parent_fails_instead_of_inventing_directories() {
        let mut fs = fs();
        assert_eq!(fs.mkdir("/home/app/sub/deep"), Err(FsError::NotFound));
        assert_eq!(fs.open("/home/app/sub/x.txt", true, false), Err(FsError::NotFound));
        fs.mkdir("/home/app/sub").unwrap();
        assert_eq!(fs.open("/home/app/sub/x.txt", true, false).unwrap(), "/home/app/sub/x.txt");
    }

    #[test]
    fn normalization_collapses_dots_and_empty_segments() {
        assert_eq!(normalize("//home///app/./"), "/home/app");
        assert_eq!(normalize("/home/app/../logs"), "/home/logs");
        assert_eq!(normalize(""), "/");
    }
}
