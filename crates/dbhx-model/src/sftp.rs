//! SFTP 面板与传输引擎的跨 IPC 形状。
//!
//! 这些类型只描述「UI 要画什么」，不含任何 russh/协议对象，因此可以脱离网络单测。

use serde::{Deserialize, Serialize};
use specta::Type;

/// 远端条目的类型。
///
/// SFTP 的 mode 位比这六种多得多（日志目录里躺着的 FIFO、`/dev` 下的字符设备都有），
/// 但前端只需要知道「能不能双击进去」，其余一律 `Other`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl FileKind {
    /// 目录可以进入。符号链接指向什么这里不知道——列目录拿到的是 lstat 的 mode 位，
    /// 解引用要另发一次 stat，属于用户双击之后的事，不在列举路径上做。
    pub fn is_enterable(&self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// 一个远端文件或目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FileEntry {
    pub name: String,
    /// 服务端风格的路径，分隔符永远是 `/`。Windows 版 OpenSSH 也用它（形如 `/C:/Users`）。
    pub path: String,
    pub kind: FileKind,
    /// 目录的 size 在多数服务端是 0 而不是缺省，前端应按 `kind` 忽略它
    pub size: u64,
    /// epoch 毫秒。服务端没给 mtime 时保持 `None`——填 0 会显示成 1970 年，
    /// 用户会以为文件真的那么旧。
    pub mtime_ms: Option<u64>,
    /// 原始 mode 低 12 位（`rwxrwxrwx` 加 setuid/setgid/sticky），供详情面板显示
    pub mode: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
}

/// 一次目录列举的结果。`path` 是服务端确认后的绝对路径，
/// 面板的地址栏应当显示它而不是用户输入的那个相对写法。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    /// 本机 → 远端
    Upload,
    /// 远端 → 本机
    Download,
}

/// 传输状态是**终态即停止**的：`Failed`/`Completed`/`Cancelled` 之后不再有该 id 的事件。
/// `Queued` 存在是因为并发文件数有上限（默认 6），排队中的任务也要显示在列表里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// 一条传输的进度快照。
///
/// 事件按 200 ms 节流，所以 `transferred` 不能用来算「精确速率」；要算速率请自己
/// 记录本地时间差。终态事件一定带最后一次计数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TransferProgress {
    pub id: u32,
    pub kind: TransferKind,
    /// 传输那一端的路径：下载是远端路径，上传是本地路径。列表里给用户看的就是它。
    pub path: String,
    pub state: TransferState,
    pub transferred: u64,
    /// `None` = 总量未知（目录还在展开，或服务端没报 size）。前端画不确定进度条。
    pub total: Option<u64>,
    /// 失败时的说明，已按 [`crate::error`] 分类成中文；其余状态恒为 `None`
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_serializes_with_snake_case_states() {
        let p = TransferProgress {
            id: 7,
            kind: TransferKind::Download,
            path: "/var/log/app.log".into(),
            state: TransferState::Running,
            transferred: 4096,
            total: Some(8192),
            error: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"state\":\"running\""), "{json}");
        assert!(json.contains("\"kind\":\"download\""), "{json}");
        assert!(json.contains("\"transferred\":4096"), "{json}");
    }

    #[test]
    fn unknown_total_survives_round_trip_as_null() {
        let entry = FileEntry {
            name: "app.log".into(),
            path: "/var/log/app.log".into(),
            kind: FileKind::File,
            size: 0,
            mtime_ms: None,
            mode: None,
            owner: None,
            group: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"mtime_ms\":null"), "{json}");
        let back: FileEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mtime_ms, None, "缺 mtime 不能变成 0");
    }

    #[test]
    fn only_directories_are_enterable() {
        assert!(FileKind::Dir.is_enterable());
        assert!(!FileKind::Symlink.is_enterable());
        assert!(!FileKind::File.is_enterable());
        assert!(!FileKind::Other.is_enterable());
    }

    #[test]
    fn terminal_states_are_terminal() {
        assert!(!TransferState::Queued.is_terminal());
        assert!(!TransferState::Running.is_terminal());
        for state in [TransferState::Completed, TransferState::Failed, TransferState::Cancelled] {
            assert!(state.is_terminal());
        }
    }
}
