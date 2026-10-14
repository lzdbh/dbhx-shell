//! 批量传输引擎：并发上限、进度事件、取消，以及「一个文件失败不拖垮整批」。
//!
//! 搬运动作抽成 [`Copier`]：生产实现是 `SftpCopier`（真读真写），测试实现是内存假数据。
//! 之所以要这层口子，是因为引擎里容易写错的从来不是「怎么搬字节」，而是编排：
//! 六路并发怎么限流、取消之后还在途的任务怎么办、某个文件失败要不要继续、
//! 面板关了就没人收事件时要不要停。这些都能在假 copier 上毫秒级跑完。
//!
//! 单文件内部的流水线不在这里：russh-sftp 的 `File` 已经把 WRITE 报文排队并发出去
//! （队列长度 = `Config::max_concurrent_writes`，见 `client::SftpOptions`），
//! 自己再开一层只会在通道里塞更多未完成请求。
//!
//! 本期**不做断点续传**：一律覆盖写。续传要先解决「远端在这期间被改过怎么办」，
//! 那需要一个指纹侧车文件，属于 M3 的事。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use dbhx_model::{TransferKind, TransferProgress, TransferState};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::error::SftpError;
use crate::path;
use crate::plan::{BoxFuture, Plan, Task};

/// 分块大小。参考实现取 32 KiB：小于它是白白多打报文，大于它会让单个 WRITE
/// 在低带宽线路上占住通道太久，交互终端会跟着卡。
pub const DEFAULT_CHUNK: usize = 32 * 1024;

/// 同时搬运的文件数。参考实现默认 6，可配 1–16。
pub const DEFAULT_MAX_FILES: usize = 6;

/// 进度事件的最小间隔。
///
/// 每个 chunk 都发一次会把 IPC 打成洪水：32 KiB 一块、百兆带宽就是每秒几千条消息，
/// 前端每收一条都要重画列表。200 ms 是人眼能接受的刷新率下限。
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// 引擎参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferOptions {
    pub chunk_size: usize,
    /// 并发文件数，1–16。超出会被夹到区间内：这个数字直接乘进通道里在途的请求数，
    /// 给个一千会把服务端压到限流。
    pub max_files: usize,
    pub progress_interval: Duration,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK,
            max_files: DEFAULT_MAX_FILES.clamp(1, 16),
            progress_interval: PROGRESS_INTERVAL,
        }
    }
}

/// 取消信号。克隆它是廉价的共享，任务持克隆、上层持原件。
#[derive(Debug, Clone)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

/// 进度节流器：决定「现在这一刻该不该发事件」。
///
/// 第一次一定发——用户按下按钮后 200 ms 内列表就该有动静，否则像是没反应。
#[derive(Debug)]
pub struct ProgressMeter {
    interval: Duration,
    last: Option<tokio::time::Instant>,
}

impl ProgressMeter {
    pub fn new(interval: Duration) -> Self {
        Self { interval, last: None }
    }

    /// 该发就返回 `true` 并重置计时。
    pub fn tick(&mut self, now: tokio::time::Instant) -> bool {
        let due = match self.last {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= self.interval,
        };
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// 一条任务的搬运动作。
pub trait Copier: Send + Sync + 'static {
    /// 把 `task` 从一端搬到另一端，每搬 `n` 字节回调一次 `progress`。
    ///
    /// `progress` 只报增量，累计由调用方做——这样节流与总量显示是一处逻辑。
    fn copy<'a>(
        &'a self,
        task: &'a Task,
        cancel: &'a Cancel,
        progress: &'a mut (dyn FnMut(u64) + Send),
    ) -> BoxFuture<'a, Result<(), SftpError>>;

    /// 起跑前把规划出来的目录先建出来。
    ///
    /// 默认什么都不做：只有真会碰到目标文件系统的实现才需要管它。
    /// 这一步是**尽力而为**——文件任务自己会建父目录，所以这里失败不该让整批停摆，
    /// 它存在的唯一理由是「勾选一个空目录」时不该静默无操作。
    fn prepare<'a>(&'a self, _plan: &'a Plan) -> BoxFuture<'a, Result<(), SftpError>> {
        Box::pin(async { Ok(()) })
    }
}

/// 生产用的 copier：真走 SFTP 与本地磁盘。
pub struct SftpCopier {
    session: Arc<SftpSession>,
    kind: TransferKind,
    chunk_size: usize,
}

impl SftpCopier {
    pub fn new(session: Arc<SftpSession>, kind: TransferKind, chunk_size: usize) -> Self {
        Self { session, kind, chunk_size }
    }
}

impl Copier for SftpCopier {
    fn copy<'a>(
        &'a self,
        task: &'a Task,
        cancel: &'a Cancel,
        progress: &'a mut (dyn FnMut(u64) + Send),
    ) -> BoxFuture<'a, Result<(), SftpError>> {
        Box::pin(async move {
            match self.kind {
                TransferKind::Download => download(self, task, cancel, progress).await,
                TransferKind::Upload => upload(self, task, cancel, progress).await,
            }
        })
    }

    fn prepare<'a>(&'a self, plan: &'a Plan) -> BoxFuture<'a, Result<(), SftpError>> {
        Box::pin(async move {
            match self.kind {
                TransferKind::Download => {
                    for dir in &plan.local_dirs {
                        // 尽力而为：文件任务自己会建父目录，这里只是为了空目录不被静默丢掉
                        if let Err(e) = tokio::fs::create_dir_all(dir).await {
                            tracing::debug!(dir = %dir.display(), ?e, "本地目录创建失败");
                        }
                    }
                }
                TransferKind::Upload => {
                    for dir in &plan.remote_dirs {
                        // SFTP 没有 `mkdir -p`，只能逐段建；已存在的段跳过——
                        // 并发任务同时建同一层是常态，把「已存在」当错误会让整批失败
                        for (_, full) in path::breadcrumb(dir) {
                            if self.session.metadata(&full).await.is_ok() {
                                continue;
                            }
                            if self.session.create_dir(&full).await.is_err() {
                                tracing::debug!(dir = %full, "远端目录创建失败");
                                break;
                            }
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

async fn download(
    copier: &SftpCopier,
    task: &Task,
    cancel: &Cancel,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), SftpError> {
    if let Some(parent) = task.local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| SftpError::local_io(&parent.display().to_string(), &e))?;
    }
    let mut remote = copier
        .session
        .open(task.remote.clone())
        .await
        .map_err(|e| SftpError::classify(e, task.remote.clone()))?;
    let mut local = tokio::fs::File::create(&task.local)
        .await
        .map_err(|e| SftpError::local_io(&task.local.display().to_string(), &e))?;

    let mut buf = vec![0u8; copier.chunk_size];
    loop {
        if cancel.is_cancelled() {
            finish_remote(remote).await;
            return Err(SftpError::Cancelled);
        }
        let n = remote.read(&mut buf).await.map_err(|e| remote_io(e, &task.remote))?;
        if n == 0 {
            break;
        }
        local.write_all(&buf[..n]).await.map_err(|e| SftpError::local_io(&task.local.display().to_string(), &e))?;
        progress(n as u64);
    }
    // 落盘完再收尾：中途 flush 失败时还能把远端句柄关掉
    local.flush().await.map_err(|e| SftpError::local_io(&task.local.display().to_string(), &e))?;
    remote.close().await.map_err(|e| remote_io(e, &task.remote))?;
    Ok(())
}

async fn upload(
    copier: &SftpCopier,
    task: &Task,
    cancel: &Cancel,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), SftpError> {
    let local_path = task.local.display().to_string();
    let mut local = tokio::fs::File::open(&task.local).await.map_err(|e| SftpError::local_io(&local_path, &e))?;
    // `create` = WRITE|CREATE|TRUNCATE：与「不做续传、一律覆盖」的口径一致
    let mut remote = copier
        .session
        .create(task.remote.clone())
        .await
        .map_err(|e| SftpError::classify(e, task.remote.clone()))?;

    let mut buf = vec![0u8; copier.chunk_size];
    loop {
        if cancel.is_cancelled() {
            finish_remote(remote).await;
            return Err(SftpError::Cancelled);
        }
        let n = local.read(&mut buf).await.map_err(|e| SftpError::local_io(&local_path, &e))?;
        if n == 0 {
            break;
        }
        // 这里刻意不 flush：russh-sftp 的写队列会自动攒够并发度再等应答，
        // 每块都 flush 就等于把流水线拆成串行
        remote.write_all(&buf[..n]).await.map_err(|e| remote_io(e, &task.remote))?;
        progress(n as u64);
    }
    // flush 把在途的 WRITE 应答收干净，close 才真的发 SSH_FXP_CLOSE
    remote.flush().await.map_err(|e| remote_io(e, &task.remote))?;
    remote.close().await.map_err(|e| remote_io(e, &task.remote))?;
    Ok(())
}

/// 取消路径上关远端句柄。关掉失败也不值得再报错——我们已经在报「已取消」了。
async fn finish_remote(mut remote: russh_sftp::client::fs::File) {
    let _ = remote.flush().await;
    let _ = remote.close().await;
}

/// 远端 IO 错误只按 `kind` 归类，**不带** `Display` 文本：
/// russh-sftp 把协议错误包成 `io::Error` 时那句英文就在 Display 里。
/// 文件句柄上的方法（`close`/`flush`）走的是 `std::io` 而不是协议 `Error`，
/// 客户端层的元数据操作也要用同一套分类，所以这一处对全 crate 开放。
pub(crate) fn remote_io(err: std::io::Error, target: &str) -> SftpError {
    match err.kind() {
        std::io::ErrorKind::NotFound => SftpError::NotFound(target.to_string()),
        std::io::ErrorKind::PermissionDenied => SftpError::PermissionDenied(target.to_string()),
        std::io::ErrorKind::AlreadyExists => SftpError::AlreadyExists(target.to_string()),
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::BrokenPipe => SftpError::Closed,
        _ => SftpError::Io(target.to_string()),
    }
}

/// 一批传输的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReport {
    pub id: u32,
    pub completed: usize,
    pub failed: usize,
    /// 取消导致的提前结束，与 `failed` 分开算：用户按了停止不该显示成「N 个失败」
    pub cancelled: usize,
    pub bytes: u64,
}

/// 一批在跑的传输。
pub struct Batch {
    id: u32,
    cancel: Cancel,
    /// `Option` 是为了 `joined` 能把句柄取出来等，同时保留 `Drop`（`Drop` 类型不许移出字段）。
    /// `None` 只可能是「已经等过了」。
    join: Option<JoinHandle<BatchReport>>,
}

impl Batch {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 取一份取消凭证。上层把它存进登记表，之后不必再碰 `Batch` 本体
    /// （`Batch` 必须一直活着，它的 `Drop` 就是停止信号）。
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    /// 等这批跑完。丢弃返回值也安全：任务自己会结束。
    pub async fn joined(mut self) -> Result<BatchReport, SftpError> {
        match self.join.take() {
            Some(join) => join.await.map_err(|_| SftpError::Internal("传输任务被运行时中止".into())),
            None => Err(SftpError::Internal("这批已经等过了".into())),
        }
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        // 面板被关掉时 Batch 往往先于任务结束被丢弃。不取消的话，任务会继续搬运并
        // 往一个没人收的通道里塞事件，白耗一条通道的带宽。
        self.cancel.cancel();
        if let Some(join) = &self.join {
            join.abort();
        }
    }
}

/// 引擎：持有任务 id 与批次 id 的发号器。
///
/// 一个进程里一个就够，克隆它没有意义（`Arc` 由上层包）。
pub struct Engine {
    options: TransferOptions,
    next_batch: AtomicU32,
    next_item: AtomicU32,
}

impl Engine {
    pub fn new(options: TransferOptions) -> Self {
        Self { options, next_batch: AtomicU32::new(1), next_item: AtomicU32::new(1) }
    }

    pub fn options(&self) -> &TransferOptions {
        &self.options
    }

    /// 起跑一批。`sink` 是进度事件出口，建议给一个有界通道：
    /// 满了说明 webview 抽不干了，此时**停下整批**比把内存吃掉更划算。
    pub fn start(&self, copier: Arc<dyn Copier>, plan: Plan, sink: mpsc::Sender<TransferProgress>) -> Batch {
        let id = self.next_batch.fetch_add(1, Ordering::Relaxed);
        let cancel = Cancel::new();
        let options = self.options;
        let base_item = self.next_item.fetch_add(plan.tasks.len() as u32, Ordering::Relaxed);
        let join = tokio::spawn(run_batch(id, base_item, copier, plan, options, cancel.clone(), sink));
        Batch { id, cancel, join: Some(join) }
    }
}

async fn run_batch(
    id: u32,
    base_item: u32,
    copier: Arc<dyn Copier>,
    plan: Plan,
    options: TransferOptions,
    cancel: Cancel,
    sink: mpsc::Sender<TransferProgress>,
) -> BatchReport {
    let mut report = BatchReport { id, completed: 0, failed: 0, cancelled: 0, bytes: 0 };
    // 目录先建出来：勾选一个空目录时，如果只等文件任务去建父目录，这批会静默地什么也不做
    let _ = copier.prepare(&plan).await;
    let kind = plan.kind;
    let tasks = plan.tasks;
    // 先把整批登记出去：列表里立刻出现所有条目（排队态），用户才知道排了多少
    for (offset, task) in tasks.iter().enumerate() {
        let event = TransferProgress {
            id: base_item + offset as u32,
            kind,
            path: display_path(kind, task),
            state: TransferState::Queued,
            transferred: 0,
            total: Some(task.size),
            error: None,
        };
        if sink.send(event).await.is_err() {
            // 没人收了（面板关了/退出中），停
            cancel.cancel();
            return report;
        }
    }

    let slots = Arc::new(Semaphore::new(options.max_files.max(1)));
    let mut running = Vec::with_capacity(tasks.len());
    for (offset, task) in tasks.into_iter().enumerate() {
        if cancel.is_cancelled() {
            // 取消之后排队的条目不再起跑。它们一条事件都不会有，
            // 由前端的超时/刷新收敛——补发一堆 Cancelled 只会把刚关的窗口再吵醒
            break;
        }
        // 许可拿不到就在这里等：这是并发上限的唯一实现点
        let Ok(permit) = Arc::clone(&slots).acquire_owned().await else { break };
        // 拿到许可后再看一次取消：等许可的那段时间里用户可能已经按了停止。
        // 不补这一眼，队列里的任务会在取消之后逐个醒来空跑一趟。
        if cancel.is_cancelled() {
            break;
        }
        let copier = Arc::clone(&copier);
        let cancel = cancel.clone();
        let sink = sink.clone();
        running.push(tokio::spawn(async move {
            let _permit = permit;
            run_task(id, base_item + offset as u32, copier, task, kind, options, cancel, sink).await
        }));
    }

    for handle in running {
        match handle.await {
            Ok(Outcome::Done(bytes)) => {
                report.completed += 1;
                report.bytes += bytes;
            }
            Ok(Outcome::Failed) => report.failed += 1,
            Ok(Outcome::Cancelled(bytes)) => {
                report.cancelled += 1;
                report.bytes += bytes;
            }
            // panic 或运行时中止：按失败计数，剩下的任务不受影响
            Err(_) => report.failed += 1,
        }
    }
    report
}

enum Outcome {
    Done(u64),
    Failed,
    Cancelled(u64),
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    batch_id: u32,
    item_id: u32,
    copier: Arc<dyn Copier>,
    task: Task,
    kind: TransferKind,
    options: TransferOptions,
    cancel: Cancel,
    sink: mpsc::Sender<TransferProgress>,
) -> Outcome {
    let path = display_path(kind, &task);
    let total = task.size;
    let mut transferred: u64 = 0;
    let mut meter = ProgressMeter::new(options.progress_interval);
    let emit = |state: TransferState, transferred: u64, error: Option<String>| TransferProgress {
        id: item_id,
        kind,
        path: path.clone(),
        state,
        transferred,
        total: Some(total),
        error,
    };
    if sink.send(emit(TransferState::Running, 0, None)).await.is_err() {
        cancel.cancel();
        return Outcome::Cancelled(transferred);
    }

    let result = copier
        .copy(
            &task,
            &cancel,
            &mut |delta| {
                transferred += delta;
                if meter.tick(tokio::time::Instant::now()) {
                    // 节流窗口外直接丢：进度事件是「示意」，丢一条不影响正确性，
                    // 而 try_send 满时阻塞在这个回调里会把整条流水线钉住
                    let _ = sink.try_send(emit(TransferState::Running, transferred, None));
                }
            },
        )
        .await;

    match result {
        Ok(()) => {
            // 终态事件一定发出去，哪怕还在节流窗口里：前端靠它把条目从「传输中」摘掉
            if sink.send(emit(TransferState::Completed, transferred, None)).await.is_err() {
                tracing::debug!(batch_id, item_id, "进度接收端已关闭");
            }
            Outcome::Done(transferred)
        }
        Err(SftpError::Cancelled) => {
            let _ = sink.send(emit(TransferState::Cancelled, transferred, None)).await;
            Outcome::Cancelled(transferred)
        }
        Err(e) => {
            let message: dbhx_model::IpcError = e.into();
            let _ = sink.send(emit(TransferState::Failed, transferred, Some(message.message))).await;
            Outcome::Failed
        }
    }
}

/// 列表里显示的那一端路径：下载看远端，上传看本地。
fn display_path(kind: TransferKind, task: &Task) -> String {
    match kind {
        TransferKind::Download => task.remote.clone(),
        TransferKind::Upload => task.local.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::mpsc;
    use tokio::time::{Duration, Instant};

    fn task(remote: &str, size: u64) -> Task {
        Task { remote: remote.into(), local: PathBuf::from("/tmp/out"), size }
    }

    fn plan(tasks: Vec<Task>) -> Plan {
        let total = tasks.iter().map(|t| t.size).sum();
        Plan {
            kind: TransferKind::Download,
            tasks,
            local_dirs: Vec::new(),
            remote_dirs: Vec::new(),
            total_bytes: total,
        }
    }

    /// 假 copier：按 `chunk` 分块、每块之间睡 `gap`，可注入失败与取消观察。
    struct FakeCopier {
        chunk: u64,
        gap: Duration,
        fail_on: Option<String>,
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl FakeCopier {
        fn new() -> Self {
            Self {
                chunk: 10,
                gap: Duration::from_millis(50),
                fail_on: None,
                in_flight: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Copier for FakeCopier {
        fn copy<'a>(
            &'a self,
            t: &'a Task,
            cancel: &'a Cancel,
            progress: &'a mut (dyn FnMut(u64) + Send),
        ) -> BoxFuture<'a, Result<(), SftpError>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(t.remote.clone());
                let live = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
                self.peak.fetch_max(live, Ordering::AcqRel);
                let mut done = 0;
                while done < t.size {
                    tokio::time::sleep(self.gap).await;
                    if cancel.is_cancelled() {
                        self.in_flight.fetch_sub(1, Ordering::AcqRel);
                        return Err(SftpError::Cancelled);
                    }
                    let n = self.chunk.min(t.size - done);
                    done += n;
                    progress(n);
                }
                if let Some(bad) = &self.fail_on {
                    if *bad == t.remote {
                        self.in_flight.fetch_sub(1, Ordering::AcqRel);
                        return Err(SftpError::PermissionDenied(bad.clone()));
                    }
                }
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                Ok(())
            })
        }
    }

    /// 起一批并把事件全收下来。
    async fn drive(options: TransferOptions, copier: Arc<FakeCopier>, plan: Plan) -> (BatchReport, Vec<TransferProgress>) {
        let (tx, mut rx) = mpsc::channel::<TransferProgress>(256);
        let engine = Engine::new(options);
        let batch = engine.start(copier, plan, tx);
        let report = batch.joined().await.unwrap();
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        (report, events)
    }

    fn options(max_files: usize) -> TransferOptions {
        TransferOptions { chunk_size: 10, max_files, progress_interval: PROGRESS_INTERVAL }
    }

    #[tokio::test(start_paused = true)]
    async fn batch_reports_per_item_lifecycle() {
        let (_, events) = drive(
            options(6),
            Arc::new(FakeCopier::new()),
            plan(vec![task("/a.log", 20)]),
        )
        .await;
        let states: Vec<TransferState> = events.iter().map(|e| e.state).collect();
        assert_eq!(states[0], TransferState::Queued, "先排队，前端才知道总量");
        assert!(states.contains(&TransferState::Running));
        assert_eq!(*states.last().unwrap(), TransferState::Completed, "终态必须收尾");
        assert_eq!(events.last().unwrap().transferred, 20);
        assert_eq!(events.last().unwrap().total, Some(20));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_files_are_capped_at_max() {
        // 8 个任务、上限 3：同时在跑的必须不超过 3
        let copier = Arc::new(FakeCopier::new());
        let tasks: Vec<Task> = (0..8).map(|i| task(&format!("/{i}.log"), 20)).collect();
        let peak = Arc::clone(&copier.peak);
        let (report, events) = drive(options(3), copier, plan(tasks)).await;
        assert_eq!(peak.load(Ordering::Acquire), 3, "并发上限是唯一实现点，不能被绕过");
        assert_eq!(report.completed, 8);
        assert_eq!(report.bytes, 160);
        // 每个条目一条终态事件
        let done = events.iter().filter(|e| e.state == TransferState::Completed).count();
        assert_eq!(done, 8);
    }

    #[tokio::test(start_paused = true)]
    async fn one_failure_does_not_stop_the_batch() {
        let mut copier = FakeCopier::new();
        copier.fail_on = Some("/bad.log".into());
        let copier = Arc::new(copier);
        let seen = Arc::clone(&copier.seen);
        let (report, events) = drive(
            options(6),
            copier,
            plan(vec![task("/bad.log", 10), task("/good.log", 10)]),
        )
        .await;
        assert_eq!((report.completed, report.failed), (1, 1));
        let names: Vec<String> = seen.lock().unwrap().clone();
        assert!(names.contains(&"/good.log".to_string()), "坏文件不该拦住后面的");
        let failed = events.iter().find(|e| e.state == TransferState::Failed).unwrap();
        assert_eq!(failed.path, "/bad.log");
        assert!(failed.error.as_deref().unwrap().contains("/bad.log"), "要看得出是哪个文件");
        assert!(!failed.error.as_deref().unwrap().contains("Permission"), "{}", failed.error.as_deref().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_stops_new_tasks_and_marks_running_ones_cancelled() {
        let copier = Arc::new(FakeCopier::new());
        let seen = Arc::clone(&copier.seen);
        let (tx, mut rx) = mpsc::channel::<TransferProgress>(256);
        let engine = Engine::new(options(1));
        let tasks = vec![task("/first.log", 40), task("/second.log", 40)];
        let batch = engine.start(Arc::clone(&copier) as Arc<dyn Copier>, plan(tasks), tx);
        // 等第一个真的跑起来，再取消
        tokio::time::sleep(Duration::from_millis(60)).await;
        batch.cancel();
        let report = batch.joined().await.unwrap();
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(report.cancelled >= 1, "{report:?}");
        assert_eq!(report.completed, 0);
        let started = seen.lock().unwrap().clone();
        assert_eq!(started, vec!["/first.log".to_string()], "并发 1 时第二个不该被起跑");
        assert!(events.iter().any(|e| e.state == TransferState::Cancelled));
        assert!(events.iter().all(|e| e.kind == TransferKind::Download));
    }

    #[tokio::test(start_paused = true)]
    async fn progress_events_are_throttled_but_terminal_always_fires() {
        // 每 50 ms 一块、间隔阈值 200 ms：一个 100 字节的文件（10 块、500 ms）
        // 只该有约两三条中间进度，最后一定有一条 Completed
        let mut copier = FakeCopier::new();
        copier.gap = Duration::from_millis(50);
        let (_, events) = drive(options(1), Arc::new(copier), plan(vec![task("/a.log", 100)])).await;
        let running = events.iter().filter(|e| e.state == TransferState::Running).count();
        assert!((2..=4).contains(&running), "200 ms 节流下 500 ms 的传输不该有 {running} 条进度");
        assert_eq!(*events.last().unwrap(), last_completed(100));
        let counts: Vec<u64> = events.iter().filter(|e| e.state == TransferState::Running).map(|e| e.transferred).collect();
        assert!(counts.windows(2).all(|w| w[1] > w[0]), "进度必须单调递增：{counts:?}");
    }

    fn last_completed(bytes: u64) -> TransferProgress {
        TransferProgress {
            id: 1,
            kind: TransferKind::Download,
            path: "/a.log".into(),
            state: TransferState::Completed,
            transferred: bytes,
            total: Some(bytes),
            error: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_sink_stops_the_batch() {
        // 接收端 drop = 面板关了。继续搬运就是往没人要的通道里写，白耗带宽
        let (tx, rx) = mpsc::channel::<TransferProgress>(4);
        let copier = Arc::new(FakeCopier::new());
        let seen = Arc::clone(&copier.seen);
        let engine = Engine::new(options(1));
        let batch = engine.start(copier, plan(vec![task("/a.log", 1000)]), tx);
        drop(rx);
        let report = batch.joined().await.unwrap();
        assert_eq!(report.completed, 0);
        assert!(seen.lock().unwrap().is_empty() || report.cancelled > 0, "{report:?}");
    }

    #[test]
    fn meter_fires_first_immediately_then_on_interval() {
        let base = Instant::now();
        let mut meter = ProgressMeter::new(Duration::from_millis(200));
        assert!(meter.tick(base), "第一次立刻发");
        assert!(!meter.tick(base + Duration::from_millis(199)));
        assert!(meter.tick(base + Duration::from_millis(200)));
        assert!(!meter.tick(base + Duration::from_millis(300)));
        assert!(meter.tick(base + Duration::from_millis(401)));
    }

    #[test]
    fn cancel_flag_is_shared_by_clone() {
        let cancel = Cancel::new();
        let other = cancel.clone();
        assert!(!other.is_cancelled());
        cancel.cancel();
        assert!(other.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_batch_cancels_the_work() {
        let (tx, _rx) = mpsc::channel::<TransferProgress>(64);
        let copier = Arc::new(FakeCopier::new());
        let engine = Engine::new(options(1));
        let batch = engine.start(copier, plan(vec![task("/a.log", 1000)]), tx);
        drop(batch);
        // Drop 里既取消也 abort，任务不会再继续搬字节
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[test]
    fn upload_shows_the_local_path_download_the_remote_one() {
        // 用 join 构造，分隔符按当前平台走，Linux CI 上才不会比出反斜杠
        let local = PathBuf::from("tmp").join("out").join("app.log");
        let t = Task { remote: "/srv/app.log".into(), local: local.clone(), size: 1 };
        assert_eq!(display_path(TransferKind::Upload, &t), local.display().to_string());
        assert_eq!(display_path(TransferKind::Download, &t), "/srv/app.log");
    }
}
