//! 本机续传日志：记住每个文件「上次搬到哪」，中断后从偏移处接着搬。
//!
//! 状态只落本机（应用数据目录），**不往远端写任何东西**。代价是换一台机器就续不了；
//! 换来的是远端目录里不会多出侧车文件，也不会因为清理侧车把权限/归属搞乱。
//!
//! 一条记录的「源指纹」是大小 + mtime。下载看远端、上传看本机；目标端已有的
//! 字节数必须正好等于上次记录的偏移，否则视为被人动过，整条作废、从头再来。
//! 这三条同时满足才续写，避免把新内容接在旧内容后面。

use std::path::PathBuf;
use std::sync::Mutex;

use dbhx_model::TransferKind;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
/// 条目上限。日志只服务「最近失败/取消的那几批」，留太多没有意义。
const MAX_ENTRIES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    scope: String,
    kind: TransferKind,
    remote: String,
    local: String,
    /// 源端大小与 mtime。远端是秒，本机是毫秒，按各自原始数值存。
    source_size: u64,
    source_mtime: i64,
    /// 中断时目标端已有的字节数，下次从这里续写。
    offset: u64,
}

impl Entry {
    fn matches(&self, scope: &str, kind: TransferKind, remote: &str, local: &str) -> bool {
        self.scope == scope && self.kind == kind && self.remote == remote && self.local == local
    }
}

/// 一条续传记录的身份与源指纹。参数打包成结构体，避免调用点排一串位置参数时把
/// `source_size` 和 `offset` 传反。
#[derive(Debug, Clone, Copy)]
pub struct ResumeTarget<'a> {
    pub scope: &'a str,
    pub kind: TransferKind,
    pub remote: &'a str,
    pub local: &'a str,
    pub source_size: u64,
    pub source_mtime: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    version: u32,
    entries: Vec<Entry>,
}

/// 续传日志。克隆请用 `Arc`；内部按文件路径去重，同名同源只留最新一条。
pub struct ResumeJournal {
    path: PathBuf,
    inner: Mutex<Vec<Entry>>,
}

impl ResumeJournal {
    /// 读入已有日志。文件不存在、损坏或版本不认识时从空开始——续传是锦上添花，
    /// 不能因为一份日志坏了就挡住传输。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let inner = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<JournalFile>(&raw).ok())
            .filter(|file| file.version == VERSION)
            .map(|file| file.entries)
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// 命中且源指纹与目标大小都对得上才返回偏移；否则清掉旧条目返回 0。
    pub fn offset_for(&self, target: ResumeTarget<'_>, dest_size: u64) -> u64 {
        let mut guard = self.lock();
        let Some(entry) = guard
            .iter()
            .find(|e| e.matches(target.scope, target.kind, target.remote, target.local))
        else {
            return 0;
        };
        if entry.source_size == target.source_size
            && entry.source_mtime == target.source_mtime
            && entry.offset == dest_size
            && entry.offset <= target.source_size
        {
            return entry.offset;
        }
        guard.retain(|e| !e.matches(target.scope, target.kind, target.remote, target.local));
        Self::flush(&self.path, &guard);
        0
    }

    /// 记下当前进度。调用方按固定字节间隔节流，不要每个分块都写盘。
    pub fn record(&self, target: ResumeTarget<'_>, offset: u64) {
        let mut guard = self.lock();
        let fresh = Entry {
            scope: target.scope.to_string(),
            kind: target.kind,
            remote: target.remote.to_string(),
            local: target.local.to_string(),
            source_size: target.source_size,
            source_mtime: target.source_mtime,
            offset,
        };
        match guard
            .iter_mut()
            .find(|e| e.matches(target.scope, target.kind, target.remote, target.local))
        {
            Some(existing) => *existing = fresh,
            None => guard.push(fresh),
        }
        if guard.len() > MAX_ENTRIES {
            let excess = guard.len() - MAX_ENTRIES;
            guard.drain(0..excess);
        }
        Self::flush(&self.path, &guard);
    }

    /// 文件搬完就清掉；失败/取消时保留最后一条，下次续写。
    pub fn forget(&self, scope: &str, kind: TransferKind, remote: &str, local: &str) {
        let mut guard = self.lock();
        let before = guard.len();
        guard.retain(|e| !e.matches(scope, kind, remote, local));
        if guard.len() != before {
            Self::flush(&self.path, &guard);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 先写 `.tmp` 再 rename：进程在写一半时被杀也不会留下半截 JSON。
    fn flush(path: &PathBuf, entries: &[Entry]) {
        let payload = JournalFile {
            version: VERSION,
            entries: entries.to_vec(),
        };
        let Ok(json) = serde_json::to_vec(&payload) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!(path = %path.display(), %e, "续传日志落盘失败");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbhx-resume-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("journal.json")
    }

    const SCOPE: &str = "app@db1:22";

    fn at<'a>(
        scope: &'a str,
        kind: TransferKind,
        remote: &'a str,
        local: &'a str,
        source_size: u64,
        source_mtime: i64,
    ) -> ResumeTarget<'a> {
        ResumeTarget {
            scope,
            kind,
            remote,
            local,
            source_size,
            source_mtime,
        }
    }

    #[test]
    fn record_survives_a_reopen_and_matches_only_the_same_source() {
        let path = temp_path("roundtrip");
        {
            let journal = ResumeJournal::open(&path);
            journal.record(
                at(
                    SCOPE,
                    TransferKind::Upload,
                    "/srv/a.bin",
                    "C:/t/a.bin",
                    100,
                    7,
                ),
                40,
            );
        }
        let journal = ResumeJournal::open(&path);
        assert_eq!(
            journal.offset_for(
                at(
                    SCOPE,
                    TransferKind::Upload,
                    "/srv/a.bin",
                    "C:/t/a.bin",
                    100,
                    7
                ),
                40
            ),
            40
        );
        // 源变了 / 目标被改了 / 换了主机，一律从头来
        assert_eq!(
            journal.offset_for(
                at(
                    SCOPE,
                    TransferKind::Upload,
                    "/srv/a.bin",
                    "C:/t/a.bin",
                    101,
                    7
                ),
                40
            ),
            0
        );
        assert_eq!(
            journal.offset_for(
                at(
                    SCOPE,
                    TransferKind::Upload,
                    "/srv/a.bin",
                    "C:/t/a.bin",
                    100,
                    7
                ),
                39
            ),
            0
        );
        assert_eq!(
            journal.offset_for(
                at(
                    "other@db2:22",
                    TransferKind::Upload,
                    "/srv/a.bin",
                    "C:/t/a.bin",
                    100,
                    7
                ),
                40
            ),
            0
        );
    }

    #[test]
    fn a_stale_entry_is_removed_on_mismatch() {
        let path = temp_path("stale");
        let journal = ResumeJournal::open(&path);
        journal.record(
            at(
                SCOPE,
                TransferKind::Download,
                "/var/log/a",
                "C:/t/a",
                100,
                7,
            ),
            40,
        );
        assert_eq!(
            journal.offset_for(
                at(
                    SCOPE,
                    TransferKind::Download,
                    "/var/log/a",
                    "C:/t/a",
                    100,
                    8
                ),
                40
            ),
            0
        );
        // 再查一次也不会命中：条目已经被清掉
        assert_eq!(
            journal.offset_for(
                at(
                    SCOPE,
                    TransferKind::Download,
                    "/var/log/a",
                    "C:/t/a",
                    100,
                    7
                ),
                40
            ),
            0
        );
    }

    #[test]
    fn forget_removes_the_entry_but_keeps_the_rest() {
        let path = temp_path("forget");
        let journal = ResumeJournal::open(&path);
        journal.record(at(SCOPE, TransferKind::Upload, "/a", "C:/a", 10, 1), 5);
        journal.record(at(SCOPE, TransferKind::Upload, "/b", "C:/b", 10, 1), 5);
        journal.forget(SCOPE, TransferKind::Upload, "/a", "C:/a");
        assert_eq!(
            journal.offset_for(at(SCOPE, TransferKind::Upload, "/a", "C:/a", 10, 1), 5),
            0
        );
        assert_eq!(
            journal.offset_for(at(SCOPE, TransferKind::Upload, "/b", "C:/b", 10, 1), 5),
            5
        );
    }

    #[test]
    fn a_corrupt_journal_starts_empty_instead_of_failing() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{not json").unwrap();
        let journal = ResumeJournal::open(&path);
        assert_eq!(
            journal.offset_for(at(SCOPE, TransferKind::Upload, "/a", "C:/a", 10, 1), 5),
            0
        );
    }
}
