//! 会话日志：把终端输出同时抄一份到本地文件。
//!
//! 抄的是**原始字节**，不做任何清洗。理由有两条：一是全屏程序（`top`、`vim`）的
//! 屏幕是靠光标移动重画出来的，把转义序列剥掉之后剩下的是一堆叠在一起的残骸，
//! 比留着更难读；二是留着之后还能用 `less -R`、`tmux` 之类原样重放。
//!
//! 落点只在 [`crate::hub`] 那一个读者里挂：会话事件流只有一个读者，多开一个
//! 会随机丢一半字节（见 `hub.rs` 顶部）。所以这里是「顺路抄一份」，
//! 而不是第二条订阅。
//!
//! **日志里会有口令。** 用户在密码提示符下敲的字，远端回显与否由远端决定，
//! 但 `echo` 出来的、程序写进 stderr 的，都会原样落进这个文件。这是「记录日志」
//! 这个功能本身的含义，不是漏洞；所以默认关、按会话开，且绝不自动放到别处。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dbhx_model::{ErrorCode, IpcError, SessionId};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// 一个会话的日志落点。
struct LiveLog {
    file: tokio::fs::File,
    path: PathBuf,
    bytes: u64,
}

/// 停止记录之后回给前端的结果。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct LogSummary {
    pub path: String,
    pub bytes: u64,
}

/// 正在记录的会话表。挂在 `Core` 上，命令层写、hub 读。
#[derive(Default)]
pub struct LogRegistry {
    inner: Mutex<HashMap<SessionId, LiveLog>>,
}

/// 抄写失败的完整现场：落点信息（要发给前端看的）加上一句人话。
///
/// 单独一个类型而不是 `Err(IpcError)`，是因为这条链上没有 IPC 调用方——
/// 失败是在 hub 的循环里发生的，它要做的是**发一件事**，而不是把错误往上抛。
#[derive(Debug)]
pub struct LogFailure {
    pub summary: LogSummary,
    pub reason: String,
}

impl LogFailure {
    /// 前端提示用的整句：在哪儿断的、为什么。
    pub fn message(&self) -> String {
        format!("写入 {} 失败：{}", self.summary.path, self.reason)
    }
}

impl LogRegistry {
    /// 开始记录某个会话。同一个会话重复开始一律拒绝：静默换文件会让用户以为
    /// 前半段还在同一个日志里。
    pub async fn start(&self, session: SessionId, path: PathBuf) -> Result<(), IpcError> {
        let mut map = self.inner.lock().await;
        if map.contains_key(&session) {
            return Err(IpcError::new(ErrorCode::InvalidInput, "这个会话已经在记录日志", false));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(io_error)?;
        map.insert(session, LiveLog { file, path, bytes: 0 });
        Ok(())
    }

    /// 抄一段输出。已经在记录才写，没在记录时是空操作。
    ///
    /// 失败时把落点当场摘掉并回 [`LogFailure`]：日志这一份副本断了，绝不该变成
    /// 「会话输出没了」或「按停止之后还在写」。调用方要做的只有一件事——告诉前端灯该灭。
    pub async fn tee(&self, session: SessionId, bytes: &[u8]) -> Result<(), LogFailure> {
        let mut map = self.inner.lock().await;
        let Some(log) = map.get_mut(&session) else { return Ok(()) };
        match log.file.write_all(bytes).await {
            Ok(()) => {
                log.bytes += bytes.len() as u64;
                Ok(())
            }
            Err(e) => Err(Self::abandon(&mut map, session, e.to_string())),
        }
    }

    /// 写失败时的收尾：落点当场摘掉，把「已经写进去多少」如实回出去。
    ///
    /// 单独抽出来有两个理由：一是摘除这件事必须和失败**同时**发生，写在一起才不会
    /// 有人改了一半；二是「写到一半磁盘满了」这类失败在离线测试里造不出来（也不值得
    /// 为了造它放开工作区的 `unsafe` 禁令），所以能验的这一段被单独验。
    fn abandon(map: &mut HashMap<SessionId, LiveLog>, session: SessionId, reason: String) -> LogFailure {
        let summary = map.remove(&session).map(|log| summary_of(&log)).unwrap_or_default();
        LogFailure { summary, reason }
    }

    /// 停止记录并回落点信息。没在记录时回 `NotFound`，让前端知道状态已经不一致。
    ///
    /// 返回之前先把落点 flush 一遍：tokio 的 `fs::File` 是「write 先收进自己的
    /// 内部缓冲，真正的落盘丢给阻塞线程池补」，所以最后一个 `tee` 的 `write_all`
    /// 返回时，文件里可能还没有那一段。不补这一记，用户点完停止立刻去开文件，
    /// 会看到少一截的日志；而且最后一段写失败时也没有下一个调用能替它报错。
    pub async fn stop(&self, session: SessionId) -> Result<LogSummary, IpcError> {
        let mut map = self.inner.lock().await;
        match map.remove(&session) {
            Some(mut log) => {
                if let Err(e) = log.file.flush().await {
                    let path = log.path.to_string_lossy().into_owned();
                    return Err(IpcError::new(ErrorCode::Io, format!("写入 {path} 失败：{e}"), false));
                }
                Ok(summary_of(&log))
            }
            None => Err(IpcError::new(ErrorCode::NotFound, "这个会话没有正在记录的日志", false)),
        }
    }

    /// 会话结束时的静默收尾：没有日志也不报错。
    ///
    /// 不发事件——tab 本身跟着消失了，前端的「正在记录」指示灯也一起没，
    /// 再发一条只是多此一举（见 [`crate::events::SessionLogStopped`] 的说明）。
    /// 这里同样要 flush：没人可报错，但至少别让最后一段烂在缓冲里。
    pub async fn close_quietly(&self, session: SessionId) {
        let Some(mut log) = self.inner.lock().await.remove(&session) else { return };
        if let Err(e) = log.file.flush().await {
            tracing::warn!(path = %log.path.to_string_lossy(), error = %e, "会话结束，日志最后一段没写进去");
        }
        let summary = summary_of(&log);
        tracing::debug!(path = %summary.path, bytes = summary.bytes, "会话结束，日志已收尾");
    }
}

/// `tokio::fs::File` 落盘时自己会关掉，这里只把「给用户看的路径」凑齐。
fn summary_of(log: &LiveLog) -> LogSummary {
    LogSummary { path: log.path.to_string_lossy().into_owned(), bytes: log.bytes }
}

fn io_error(e: std::io::Error) -> IpcError {
    IpcError::new(ErrorCode::Io, e.to_string(), false)
}

/// 校验前端给的文件名。
///
/// 只准是**一个名字**：带分隔符、带 `..`、带控制字符的一律拒绝，于是「日志落在
/// 哪儿」这件事始终由后端的那一个目录决定，前端只能决定叫什么。
pub fn check_file_name(name: &str) -> Result<(), IpcError> {
    let bad = name.is_empty()
        || name.len() > 120
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(|c| (c as u32) < 0x20 || c == ':');
    if bad {
        return Err(IpcError::new(ErrorCode::InvalidInput, format!("日志文件名不合法：{name}"), false));
    }
    Ok(())
}

/// 拼出日志的完整落点。`dir` 缺省时用应用数据目录下的 `logs`。
pub fn log_path(core_dir: &Path, dir: Option<&str>, name: &str) -> Result<PathBuf, IpcError> {
    check_file_name(name)?;
    let base = match dir {
        None => core_dir.join("logs"),
        Some(raw) => {
            let candidate = Path::new(raw);
            if !candidate.is_absolute() {
                return Err(IpcError::new(ErrorCode::InvalidInput, "日志目录必须是绝对路径", false));
            }
            candidate.to_path_buf()
        }
    };
    Ok(base.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: u32) -> SessionId {
        SessionId(n)
    }

    fn name() -> String {
        "dbhx-本机-20261014.log".to_string()
    }

    #[tokio::test]
    async fn logged_bytes_land_in_the_file() {
        let dir = temp_dir("tee");
        let registry = LogRegistry::default();
        let path = dir.join("a.log");
        registry.start(sid(1), path.clone()).await.unwrap();
        registry.tee(sid(1), b"hello ").await.unwrap();
        registry.tee(sid(1), b"world\n").await.unwrap();
        let summary = registry.stop(sid(1)).await.unwrap();
        assert_eq!(summary.bytes, 12);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world\n");
        clean(&dir);
    }

    #[tokio::test]
    async fn not_logging_sessions_are_a_no_op() {
        let registry = LogRegistry::default();
        registry.tee(sid(7), b"nothing").await.unwrap();
        registry.close_quietly(sid(7)).await;
    }

    #[tokio::test]
    async fn closing_at_session_end_stops_the_recording() {
        let dir = temp_dir("quiet");
        let registry = LogRegistry::default();
        registry.start(sid(8), dir.join("end.log")).await.unwrap();
        registry.tee(sid(8), b"last\n").await.unwrap();
        registry.close_quietly(sid(8)).await;
        // 收尾之后再停就什么都没有了：表里确实不再有这条记录
        assert_eq!(registry.stop(sid(8)).await.unwrap_err().code, ErrorCode::NotFound);
        // 已经落盘的内容不能被收尾动作吃掉
        assert_eq!(std::fs::read_to_string(dir.join("end.log")).unwrap(), "last\n");
        clean(&dir);
    }

    #[tokio::test]
    async fn starting_twice_is_refused_and_keeps_the_first_file() {
        let dir = temp_dir("twice");
        let registry = LogRegistry::default();
        registry.start(sid(2), dir.join("first.log")).await.unwrap();
        let again = registry.start(sid(2), dir.join("second.log")).await;
        assert!(again.is_err(), "静默换文件等于骗用户前半段还在同一个日志里");
        registry.tee(sid(2), b"x").await.unwrap();
        assert_eq!(registry.stop(sid(2)).await.unwrap().path, dir.join("first.log").to_string_lossy());
        assert!(!dir.join("second.log").exists());
        clean(&dir);
    }

    #[tokio::test]
    async fn append_keeps_what_was_there_before() {
        let dir = temp_dir("append");
        std::fs::write(dir.join("same.log"), "上一轮的\n".as_bytes()).unwrap();
        let registry = LogRegistry::default();
        registry.start(sid(3), dir.join("same.log")).await.unwrap();
        registry.tee(sid(3), "这一轮的\n".as_bytes()).await.unwrap();
        registry.stop(sid(3)).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("same.log")).unwrap(), "上一轮的\n这一轮的\n");
        clean(&dir);
    }

    #[tokio::test]
    async fn a_failed_sink_is_dropped_with_what_it_already_wrote() {
        let dir = temp_dir("abandon");
        let registry = LogRegistry::default();
        registry.start(sid(4), dir.join("gone.log")).await.unwrap();
        registry.tee(sid(4), b"first\n").await.unwrap();
        // 「磁盘满了」这类失败在离线环境造不出来，所以直接叫收尾那一步，
        // 验的是它能报出已写成的量、并且把落点摘干净
        let failure = {
            let mut map = registry.inner.lock().await;
            LogRegistry::abandon(&mut map, sid(4), "磁盘已满".to_string())
        };
        assert!(failure.summary.path.ends_with("gone.log"), "{}", failure.summary.path);
        assert_eq!(failure.summary.bytes, 6, "已经写进去的字节要如实报出来");
        assert_eq!(failure.message(), format!("写入 {} 失败：磁盘已满", failure.summary.path));

        // 摘干净之后：下一条字节是空操作，也就不再占着「正在记录」这个状态
        registry.tee(sid(4), b"second\n").await.unwrap();
        let err = registry.stop(sid(4)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        clean(&dir);
    }

    #[test]
    fn file_names_with_any_path_escape_are_refused() {
        for bad in ["", ".", "..", "a/b.log", "a\\b.log", "con:log", "a\u{7}b", &"x".repeat(121)] {
            assert!(check_file_name(bad).is_err(), "不该放过 {bad:?}");
        }
        assert!(check_file_name("dbhx-本机终端-20261014-2359.log").is_ok());
    }

    #[test]
    fn the_default_directory_is_under_the_app_data_dir() {
        let path = log_path(Path::new("/app/data"), None, &name()).unwrap();
        assert_eq!(path, PathBuf::from("/app/data/logs/dbhx-本机-20261014.log"));
    }

    #[test]
    fn a_custom_directory_must_be_absolute() {
        // 「是不是绝对路径」由平台判定，所以这里只验被拒的那一半：任何相对写法
        // 都不许悄悄落到 core_dir 旁边去。
        for bad in ["", "logs", "logs/2026", "../logs"] {
            assert!(
                log_path(Path::new("/app/data"), Some(bad), &name()).is_err(),
                "不该放过相对目录 {bad:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_directory_is_honoured_on_unix() {
        let path = log_path(Path::new("/app/data"), Some("/var/log/dbhx"), &name()).unwrap();
        assert_eq!(path, PathBuf::from("/var/log/dbhx/dbhx-本机-20261014.log"));
    }

    #[cfg(windows)]
    #[test]
    fn an_absolute_directory_is_honoured_on_windows() {
        let path = log_path(Path::new(r"C:\app\data"), Some(r"F:\logs"), &name()).unwrap();
        assert_eq!(path, PathBuf::from(r"F:\logs\dbhx-本机-20261014.log"));
    }

    /// 开不出来时如实回 `io`，而不是悄悄留一个空文件让用户以为在记。
    ///
    /// 这里用 Windows 的共享模式把目标文件独占住——这是离线环境里唯一能真实造出来的
    /// 打开失败。「写了一半才失败」（磁盘满、中途被独占）造不出来，只能靠上面那个
    /// `abandon` 用例验它的收尾账目。
    #[cfg(windows)]
    #[tokio::test]
    async fn an_unopenable_target_is_reported_as_io() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = temp_dir("locked");
        let path = dir.join("locked.log");
        let blocker = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let err = LogRegistry::default().start(sid(9), path).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        drop(blocker);
        clean(&dir);
    }

    /// 每个用例一个目录，且**先清空**：日志是追加写的，上一次跑到一半失败留下的
    /// 半个文件会让下一次的「读回来等于写入内容」莫名其妙地红。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbhx-log-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clean(dir: &Path) {
        std::fs::remove_dir_all(dir).ok();
    }
}
