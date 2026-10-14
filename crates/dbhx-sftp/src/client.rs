//! 目录列举与元数据操作。
//!
//! 边界：这里只接一条已经开好的 subsystem 裸流（`SubsystemStream`），
//! **不认识 lease、不认识 vault**。谁拿着 SSH 连接、什么时候回收，是上层的事——
//! 这条边界让本 crate 不依赖会话层，也让他处（未来的独立拨号路径）能复用同一个客户端。
//!
//! russh-sftp 的高层 `File` 内部就是流水线：`poll_write` 用 `write_nowait_from_slice`
//! 把 WRITE 报文发出去、把应答句柄排队，队列长度上限即 `max_concurrent_writes`。
//! 所以「一条通道内并行几十个读写」不需要自己实现，只要把并发度设够——
//! 参考实现的 64 路并行对应这里的 `SftpOptions::max_inflight_ops`。

use std::sync::Arc;

use dbhx_model::{DirListing, FileEntry, FileKind};
use dbhx_ssh::SubsystemStream;
use russh_sftp::client::{Config, SftpSession};
use russh_sftp::protocol::FileAttributes;

use crate::error::SftpError;
use crate::path;
use crate::transfer::remote_io;

/// 会话建连参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SftpOptions {
    /// 单条通道上允许的并行 READ/WRITE 数。参考实现取 64；
    /// 太小（比如默认的 16）会让大文件跑不满带宽，太大则会在低配堡垒机上触发服务端限流。
    pub max_inflight_ops: usize,
    /// 单个 SFTP 请求的应答超时，秒。
    ///
    /// 比 russh-sftp 默认的 10 秒长：内网跳板机在批量传输时会把单个 WRITE 拖到十几秒，
    /// 那种情况下报错并中断整个下载，比多等一会儿更糟。
    pub request_timeout_secs: u64,
}

impl Default for SftpOptions {
    fn default() -> Self {
        Self { max_inflight_ops: 64, request_timeout_secs: 30 }
    }
}

/// 一个文件面板对应一条 SFTP 会话。
///
/// 会话本身是引用计数式的（内部 `Arc<RawSftpSession>`），克隆这个客户端是廉价的共享，
/// 但**不要**为同一台主机开两条 SFTP 会话：每条都是一个 SSH channel，
/// 堡垒机往往在通道数上有限制。
#[derive(Clone)]
pub struct SftpClient {
    session: Arc<SftpSession>,
}

impl SftpClient {
    /// 用默认参数建连。会走完协议握手（`INIT`/`VERSION` + 扩展协商），
    /// 因此这一步失败说明通道根本不可用，调用方应当直接丢弃 lease。
    pub async fn connect(stream: SubsystemStream) -> Result<Self, SftpError> {
        Self::with_options(stream, SftpOptions::default()).await
    }

    pub async fn with_options(stream: SubsystemStream, options: SftpOptions) -> Result<Self, SftpError> {
        let config = Config {
            max_concurrent_reads: options.max_inflight_ops,
            max_concurrent_writes: options.max_inflight_ops,
            ..Config::default()
        };
        let session = SftpSession::new_with_config(stream, config)
            .await
            .map_err(|e| SftpError::classify(e, "sftp"))?;
        session.set_timeout(options.request_timeout_secs);
        Ok(Self { session: Arc::new(session) })
    }

    /// 交给传输引擎用。引擎只碰文件，不碰目录语义。
    pub fn session(&self) -> Arc<SftpSession> {
        Arc::clone(&self.session)
    }

    /// 让服务端给出路径的真实形态（解符号链接、展开相对写法）。
    ///
    /// 面板每次导航都先问一次：地址栏要显示服务端确认的路径，否则用户在面包屑上
    /// 点一下就会跳到符号链接的「字面位置」而不是真实位置。
    pub async fn canonicalize(&self, target: &str) -> Result<String, SftpError> {
        self.session
            .canonicalize(target)
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))
    }

    /// 进入一个目录：先确认路径，再列举并排序。
    ///
    /// 分两步是因为 `read_dir` 失败时无法区分「路径写错」和「目录不存在」，
    /// 而这两者对用户的提示完全不同。
    pub async fn browse(&self, target: &str) -> Result<DirListing, SftpError> {
        let canonical = self.canonicalize(target).await?;
        let mut entries = self.read_dir(&canonical).await?;
        sort_entries(&mut entries);
        Ok(DirListing { path: canonical, entries })
    }

    /// 列举目录，不排序、不解析路径。给「按名字直接取一项」之类的调用方留的窄口子。
    pub async fn read_dir(&self, canonical: &str) -> Result<Vec<FileEntry>, SftpError> {
        let read = self
            .session
            .read_dir(canonical.to_string())
            .await
            .map_err(|e| SftpError::classify(e, canonical.to_string()))?;
        // `ReadDir` 已经跳过 `.` 和 `..`，这里不重复过滤：
        // 它内部是把整个目录收进内存再返回迭代器，所以我们拿到的是一份完整快照。
        Ok(read.map(|entry| to_entry(entry.file_name(), entry.path(), &entry.metadata())).collect())
    }

    /// 取单个条目的元数据。跟随符号链接。
    pub async fn stat(&self, target: &str) -> Result<FileEntry, SftpError> {
        let attrs = self
            .session
            .metadata(target.to_string())
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))?;
        Ok(to_entry(path::basename(target).unwrap_or(target).to_string(), target.to_string(), &attrs))
    }

    /// 取符号链接本身的元数据（不解引用）。
    pub async fn lstat(&self, target: &str) -> Result<FileEntry, SftpError> {
        let attrs = self
            .session
            .symlink_metadata(target.to_string())
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))?;
        Ok(to_entry(path::basename(target).unwrap_or(target).to_string(), target.to_string(), &attrs))
    }

    /// 是否存在。服务端明确回答「没有」时返回 `false`，其它错误照常上抛——
    /// 把超时当成「不存在」会让覆盖逻辑删错文件。
    pub async fn exists(&self, target: &str) -> Result<bool, SftpError> {
        match self.session.try_exists(target).await {
            Ok(exists) => Ok(exists),
            Err(e) => match SftpError::classify(e, target.to_string()) {
                SftpError::NotFound(_) => Ok(false),
                other => Err(other),
            },
        }
    }

    pub async fn mkdir(&self, target: &str) -> Result<(), SftpError> {
        self.session
            .create_dir(target)
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))
    }

    /// 逐级建目录。用于上传目录树——SFTP 没有 `mkdir -p`，只能自己按段建。
    ///
    /// 已存在的段跳过：并发传输里两个任务同时建同一层是常态，
    /// 把「已存在」当错误会让整批传输莫名失败。
    pub async fn mkdir_all(&self, target: &str) -> Result<(), SftpError> {
        let canonical = path::normalize(target);
        if !path::is_absolute(&canonical) {
            return Err(SftpError::BadPath(target.to_string()));
        }
        // 根目录不需要建：breadcrumb 从第一段开始，逐段建到 target
        for (_, full) in path::breadcrumb(&canonical) {
            if !self.exists(&full).await? {
                self.mkdir(&full).await?;
            }
        }
        Ok(())
    }

    /// 建一个空文件。
    ///
    /// **已存在时报错，绝不截断**：`create` 的语义是 WRITE|CREATE|TRUNCATE，直接把名字
    /// 丢过去会把同名文件清成 0 字节，而用户在面板上敲错一个名字是常事。
    pub async fn create_file(&self, target: &str) -> Result<(), SftpError> {
        if self.exists(target).await? {
            return Err(SftpError::AlreadyExists(target.to_string()));
        }
        let file = self
            .session
            .create(target.to_string())
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))?;
        // 显式 close 而不是靠 Drop：Drop 里的 CLOSE 是 nowait 投递，
        // 命令层紧接着就 browse，那条新文件可能还没在服务端落稳
        file.close().await.map_err(|e| remote_io(e, target))?;
        Ok(())
    }

    /// 改权限位（SETATTRS），等价于 chmod。
    ///
    /// 先 `stat` 一次拿回现有属性，只替换低 12 位：`permissions` 里高位是类型位
    /// （目录/符号链接/普通文件），整段覆盖过去会让服务端把 `a.log` 变成「没有类型」的条目。
    pub async fn set_mode(&self, target: &str, mode: u32) -> Result<(), SftpError> {
        let current = self
            .session
            .metadata(target.to_string())
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))?;
        let kept = current.permissions.unwrap_or(0) & !0o7777;
        let mut attrs = FileAttributes::default();
        attrs.permissions = Some(kept | (mode & 0o7777));
        self.session
            .set_metadata(target.to_string(), attrs)
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))
    }

    pub async fn remove_file(&self, target: &str) -> Result<(), SftpError> {
        self.session
            .remove_file(target)
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))
    }

    pub async fn remove_dir(&self, target: &str) -> Result<(), SftpError> {
        self.session
            .remove_dir(target)
            .await
            .map_err(|e| SftpError::classify(e, target.to_string()))
    }

    /// 移动/重命名。
    ///
    /// `overwrite` 是必须显式传的：SFTP v3 的 `SSH_FXP_RENAME` 在 OpenSSH 上**不会**覆盖
    /// 已有目标，而用户点「移到这里」时期待的是覆盖。做法是先删后移，代价是有极短的
    /// 空窗；目录目标一律拒绝，避免把同名目录整个删掉。
    pub async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), SftpError> {
        if from == to {
            return Ok(());
        }
        if overwrite && self.exists(to).await? {
            let target = self.stat(to).await?;
            if target.kind == FileKind::Dir {
                return Err(SftpError::AlreadyExists(to.to_string()));
            }
            self.remove_file(to).await?;
        }
        self.session
            .rename(from, to)
            .await
            .map_err(|e| SftpError::classify(e, format!("{from} → {to}")))
    }

    /// 显式结束会话（发 `SSH_FXP_STATUS` 关闭）。面板软关闭时调用，
    /// 它不影响底层 SSH 连接——那是 lease 的事。
    pub async fn close(&self) -> Result<(), SftpError> {
        self.session
            .close()
            .await
            .map_err(|e| SftpError::classify(e, "close"))
    }
}

/// 目录内排序：目录在前，同类按名字（不区分大小写）。
///
/// 排序放在 Rust 侧而不是让前端做，是为了让「同名文件的兄弟目录」在所有面板里
/// 位置一致；前端只按这个顺序渲染，不再自己排。
pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        let dir_first = |kind: FileKind| match kind {
            FileKind::Dir => 0u8,
            FileKind::Symlink => 1,
            FileKind::File => 2,
            FileKind::Other => 3,
        };
        dir_first(a.kind)
            .cmp(&dir_first(b.kind))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            // 大小写不敏感会把 "log" 和 "LOG" 判成同一项，补一次按原名字比较，
            // 否则同一批文件每次刷新顺序都可能变
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// 把服务端的属性报文转成面板条目。
///
/// 所有字段都是 `Option`：协议里允许服务端什么都不说。缺的一律留 `None`，
/// 不要填 0——填 0 会让 UI 显示「1970 年、0 字节」，用户会当成文件真的如此。
fn to_entry(name: String, full_path: String, attrs: &FileAttributes) -> FileEntry {
    // OpenSSH/dropbear 在 readdir 里一定带 mode 位；真的没带（某些代理型设备）就只能算 Other，
    // 此时面板不给「进入」，比猜错强。
    let kind = match attrs.permissions {
        Some(_) => match attrs.file_type() {
            russh_sftp::protocol::FileType::Dir => FileKind::Dir,
            russh_sftp::protocol::FileType::File => FileKind::File,
            russh_sftp::protocol::FileType::Symlink => FileKind::Symlink,
            russh_sftp::protocol::FileType::Other => FileKind::Other,
        },
        None => FileKind::Other,
    };
    FileEntry {
        name,
        path: full_path,
        kind,
        size: attrs.size.unwrap_or(0),
        mtime_ms: attrs.mtime.map(|secs| u64::from(secs) * 1000),
        mode: attrs.permissions.map(|perm| perm & 0o7777),
        owner: attrs.user.clone().filter(|s| !s.is_empty()),
        group: attrs.group.clone().filter(|s| !s.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(permissions: Option<u32>, size: Option<u64>, mtime: Option<u32>) -> FileAttributes {
        FileAttributes {
            size,
            uid: Some(0),
            user: None,
            gid: Some(0),
            group: None,
            permissions,
            atime: mtime,
            mtime,
        }
    }

    fn entry(name: &str, kind: FileKind) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: format!("/{name}"),
            kind,
            size: 0,
            mtime_ms: None,
            mode: None,
            owner: None,
            group: None,
        }
    }

    #[test]
    fn mode_bits_map_to_the_four_kinds() {
        // 0o040000 DIR、0o010000 REG、0o012000 LNK，其余算 Other
        let dir = to_entry("logs".into(), "/logs".into(), &attrs(Some(0o040755), Some(4096), None));
        assert_eq!(dir.kind, FileKind::Dir);
        assert_eq!(dir.mode, Some(0o755), "setuid 之外的模式位不该进 UI 字段");

        let file = to_entry("a.bin".into(), "/a.bin".into(), &attrs(Some(0o0100644), Some(7), None));
        assert_eq!(file.kind, FileKind::File);
        assert_eq!(file.mode, Some(0o644));

        let link = to_entry("current".into(), "/current".into(), &attrs(Some(0o0120777), Some(11), None));
        assert_eq!(link.kind, FileKind::Symlink);

        let fifo = to_entry("x".into(), "/x".into(), &attrs(Some(0o0010666), Some(0), None));
        assert_eq!(fifo.kind, FileKind::Other);
    }

    #[test]
    fn missing_attributes_stay_missing() {
        let e = to_entry("mystery".into(), "/mystery".into(), &attrs(None, None, None));
        assert_eq!(e.kind, FileKind::Other, "没给 mode 就不能假装它是文件");
        assert_eq!(e.mode, None);
        assert_eq!(e.mtime_ms, None, "缺 mtime 绝不能变成 1970");
        assert_eq!(e.size, 0);
    }

    #[test]
    fn mtime_seconds_become_milliseconds() {
        let e = to_entry("a".into(), "/a".into(), &attrs(Some(0o0100644), Some(1), Some(1_700_000_000)));
        assert_eq!(e.mtime_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn empty_owner_strings_are_treated_as_absent() {
        let mut a = attrs(Some(0o0100644), Some(1), None);
        a.user = Some(String::new());
        a.group = Some("deploy".into());
        let e = to_entry("a".into(), "/a".into(), &a);
        assert_eq!(e.owner, None, "空串会让表格显示一个空白列");
        assert_eq!(e.group.as_deref(), Some("deploy"));
    }

    #[test]
    fn directories_sort_first_then_case_insensitively() {
        let mut entries = vec![
            entry("zeta.log", FileKind::File),
            entry("Bin", FileKind::Dir),
            entry("apple", FileKind::File),
            entry("app", FileKind::Dir),
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["app", "Bin", "apple", "zeta.log"]);
    }

    #[test]
    fn case_only_difference_is_still_stable() {
        let mut entries = vec![entry("LOG", FileKind::File), entry("log", FileKind::File)];
        sort_entries(&mut entries);
        assert_eq!(entries[0].name, "LOG", "大小写不敏感比较后必须再比一次原名，否则刷新会换序");
    }

    #[test]
    fn symlinks_group_between_dirs_and_files() {
        let mut entries = vec![
            entry("readme", FileKind::File),
            entry("latest", FileKind::Symlink),
            entry("conf", FileKind::Dir),
            entry("core", FileKind::Other),
        ];
        sort_entries(&mut entries);
        let kinds: Vec<FileKind> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [FileKind::Dir, FileKind::Symlink, FileKind::File, FileKind::Other]);
    }
}
