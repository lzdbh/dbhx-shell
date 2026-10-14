//! SFTP 协议服务端：把 [`crate::vfs::Vfs`] 接到 `russh-sftp` 的报文层。
//!
//! 只实现被测代码真正会发的那些报文，其余一律 `OP_UNSUPPORTED`——
//! 假装支持反而会让客户端走进我们从未验证过的分支。
//!
//! 有一处必须留意：`readdir` 的 `filename` 是**basename**，不是全路径。
//! 客户端自己用「父目录 + basename」拼出完整路径，返回全路径会让面板里的条目
//! 变成 `/a//a/b` 这种畸形串，而且看起来「有数据」，错误很难一眼看出来。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};

use crate::vfs::{FsError, Kind, Stat, Vfs};

/// 一个句柄指向的东西。目录句柄记住「是否已经吐完」，因为客户端会反复 `readdir` 直到 EOF。
#[derive(Debug, Clone)]
enum Opened {
    File { path: String },
    Dir { path: String, drained: bool },
}

pub struct SftpHandler {
    fs: Arc<Mutex<Vfs>>,
    /// 会话级 cwd。真实 `sftp-server` 起来就在用户家目录，相对路径按这里展开，
    /// 所以夹具也必须从家目录开始，否则客户端发的相对写法会指向完全不同的地方。
    cwd: String,
    handles: HashMap<String, Opened>,
    next_handle: u32,
}

impl SftpHandler {
    pub fn new(fs: Arc<Mutex<Vfs>>, cwd: impl Into<String>) -> Self {
        Self { fs, cwd: cwd.into(), handles: HashMap::new(), next_handle: 0 }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vfs> {
        // 夹具里的 panic 只可能来自测试代码自身，恢复内部数据继续跑比整个服务器崩掉有用
        self.fs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 报文里的路径 ⇒ 规范化绝对路径。
    fn abs(&self, raw: &str) -> Result<String, StatusCode> {
        let fs = self.lock();
        fs.resolve(raw, &self.cwd).map_err(to_status)
    }

    /// 已经规范化过的路径：只做存在性与类型检查，返回快照。
    fn stat_of(&self, path: &str, follow: bool) -> Result<Stat, StatusCode> {
        self.lock().stat(path, follow).map_err(to_status)
    }

    fn take_file(&self, handle: &str) -> Result<String, StatusCode> {
        match self.handles.get(handle) {
            Some(Opened::File { path }) => Ok(path.clone()),
            _ => Err(StatusCode::BadMessage),
        }
    }
}

fn to_status(err: FsError) -> StatusCode {
    match err {
        FsError::NotFound => StatusCode::NoSuchFile,
        FsError::Exists => StatusCode::Failure,
        FsError::NotDir | FsError::IsDir => StatusCode::PermissionDenied,
        FsError::BadPath => StatusCode::BadMessage,
        FsError::Loop => StatusCode::Failure,
    }
}

fn ok(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: String::new(), language_tag: "en".to_string() }
}

/// 把 VFS 的快照写成 SFTP 的属性位图。
///
/// `permissions` 里必须带上**类型位**（`0o40000` 目录、`0o10000` 符号链接等），
/// 客户端就是靠这几个位区分目录/文件/链接的；只写 rwxx 权限位会让面板把目录列成文件。
fn attrs_of(node: &Stat) -> FileAttributes {
    let mut attrs = FileAttributes::default();
    attrs.size = Some(node.size);
    attrs.uid = Some(1000);
    attrs.gid = Some(1000);
    attrs.permissions = Some(node.mode | type_bits(node.kind));
    attrs.atime = Some(node.mtime);
    attrs.mtime = Some(node.mtime);
    attrs
}

fn type_bits(kind: Kind) -> u32 {
    match kind {
        Kind::Dir => 0o040000,
        Kind::Symlink => 0o120000,
        Kind::File => 0o100000,
    }
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, Self::Error> {
        // 报 `pwd` 扩展的存在：客户端的 canonicalize 会优先走它，
        // 不给的话 realpath 也能工作，但那条分支就永远测不到。
        let mut version = Version::new();
        version.extensions.insert("posix-rename@openssh.com".to_string(), "1".to_string());
        Ok(version)
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let abs = self.abs(&path)?;
        let node = self.stat_of(&abs, true)?;
        let attrs = attrs_of(&node);
        Ok(Name { id, files: vec![File::new(node.path, attrs)] })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let node = self.stat_of(&self.abs(&path)?, true)?;
        Ok(Attrs { id, attrs: attrs_of(&node) })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let node = self.stat_of(&self.abs(&path)?, false)?;
        Ok(Attrs { id, attrs: attrs_of(&node) })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let path = self.take_file(&handle)?;
        let node = self.stat_of(&path, true)?;
        Ok(Attrs { id, attrs: attrs_of(&node) })
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, _attrs: FileAttributes) -> Result<Handle, Self::Error> {
        let abs = self.abs(&filename)?;
        let create = pflags.contains(OpenFlags::CREATE);
        let truncate = pflags.contains(OpenFlags::TRUNCATE);
        let final_path = {
            let mut fs = self.lock();
            fs.open(&abs, create, truncate).map_err(to_status)?
        };
        let handle = self.next_handle_name();
        self.handles.insert(handle.clone(), Opened::File { path: final_path });
        Ok(Handle { id, handle })
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let path = self.take_file(&handle)?;
        let bytes = self.lock().read_at(&path, offset, len).map_err(to_status)?;
        // 空返回就是 EOF 的意思，但报文层要求显式报错，客户端据此收尾
        if bytes.is_empty() && len > 0 {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data: bytes })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let path = self.take_file(&handle)?;
        self.lock().write_at(&path, offset, &data).map_err(to_status)?;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let abs = self.abs(&path)?;
        self.lock().opendir(&abs).map_err(to_status)?;
        let handle = self.next_handle_name();
        self.handles.insert(handle.clone(), Opened::Dir { path: abs, drained: false });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Opened::Dir { path, drained } = self.handles.get(&handle).cloned().ok_or(StatusCode::BadMessage)? else {
            return Err(StatusCode::BadMessage);
        };
        if drained {
            return Err(StatusCode::Eof);
        }
        let listing = self.lock().list(&path);
        // 一次吐完，下一次调用就该是 EOF——客户端的 read_dir 正是这么收尾的
        if let Some(entry) = self.handles.get_mut(&handle) {
            if let Opened::Dir { drained, .. } = entry {
                *drained = true;
            }
        }
        Ok(Name {
            id,
            files: listing
                .into_iter()
                .map(|(full, node)| {
                    let name = full.rsplit('/').next().unwrap_or(&full).to_string();
                    File::new(name, attrs_of(&node))
                })
                .collect(),
        })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let abs = self.abs(&filename)?;
        self.lock().remove_file(&abs).map_err(to_status)?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        let abs = self.abs(&path)?;
        self.lock().mkdir(&abs).map_err(to_status)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let abs = self.abs(&path)?;
        self.lock().remove_dir(&abs).map_err(to_status)?;
        Ok(ok(id))
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        let from = self.abs(&oldpath)?;
        let to = self.abs(&newpath)?;
        self.lock().rename(&from, &to).map_err(to_status)?;
        Ok(ok(id))
    }

    async fn symlink(&mut self, id: u32, linkpath: String, targetpath: String) -> Result<Status, Self::Error> {
        // OpenSSH 的两个参数顺序和直觉相反（linkpath 在前），客户端按同一顺序发，
        // 这里保持原样，写反了测试也发现不了——所以把顺序写进注释。
        let link = self.abs(&linkpath)?;
        let target = self.abs(&targetpath)?;
        self.lock().symlink(&link, &target).map_err(to_status)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let abs = self.abs(&path)?;
        let target = self.lock().read_link(&abs).map_err(to_status)?;
        Ok(Name { id, files: vec![File::new(target, FileAttributes::default())] })
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        let abs = self.abs(&path)?;
        // 两个字段各管各的：只带 permissions 的 SETATTRS 就是 chmod，只带 size 就是截断，
        // 缺的那个字段绝不能顺手改动（客户端改权限时把 mtime 报成 0 就是这么来的）
        let mut fs = self.lock();
        if let Some(mode) = attrs.permissions {
            fs.set_mode(&abs, mode).map_err(to_status)?;
        }
        if let Some(size) = attrs.size {
            fs.set_size(&abs, size).map_err(to_status)?;
        }
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        let path = self.take_file(&handle)?;
        let mut fs = self.lock();
        if let Some(mode) = attrs.permissions {
            fs.set_mode(&path, mode).map_err(to_status)?;
        }
        if let Some(size) = attrs.size {
            fs.set_size(&path, size).map_err(to_status)?;
        }
        Ok(ok(id))
    }
}

impl SftpHandler {
    fn next_handle_name(&mut self) -> String {
        self.next_handle = self.next_handle.wrapping_add(1);
        format!("h{}", self.next_handle)
    }
}
