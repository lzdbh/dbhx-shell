//! 本地 PTY 会话：三个阻塞任务 + 两条通道。
//!
//! portable-pty 整套 API 都是阻塞的（`Read`/`Write`/`try_wait`），而我们的运行时是
//! tokio，所以每一侧都只能放进 `spawn_blocking`：
//!
//! * reader：把 pty 输出搬进事件通道，顺带代答 ConPTY 的 DSR 查询；
//! * writer：把键盘输入搬给子进程；
//! * owner：独占 `master` 与 `child`，只负责 resize、kill 和探测退出。
//!
//! owner 之所以必须独立存在，是类型层面的约束：`MasterPty: Downcast + Send` 并**不是**
//! `Sync`，于是 `Box<dyn MasterPty + Send>` 既不能放进 `Arc` 共享给会话注册表，也不能
//! 在两个线程间同时持有。尺寸变化只能投递给它的主人。
//!
//! 三个任务都不保留 `JoinHandle`：它们的结束条件全是「通道断开、子进程退出或读端 EOF」，
//! 句柄存活期由 `LocalSession` 的字段决定，比记住三个 handle 再逐个 abort 可靠得多
//! （`abort` 对阻塞任务本来就不生效）。

use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbhx_model::PtySize;
use portable_pty::{native_pty_system, Child, ChildKiller, MasterPty};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::error::PtyError;
use crate::spec::{to_wire_size, LocalSpec};

/// 输入队列长度。按键体积很小，256 条足够吸收一次 UI 抖动；
/// 再深就说明子进程已经不读了，此时丢弃比堆积有用。
const STDIN_QUEUE: usize = 256;

/// 两个阻塞任务的空闲轮询间隔（owner 探退出、writer 探存活标志）。
/// 有命令或输入投递时它们会立刻被唤醒，所以这个值只影响「没人通知」那几条路径：
/// 太大就拖长了退出与 EOF 的传播，太小就白烧 CPU；20 ms 在终端场景里两头都够。
const IDLE_POLL: Duration = Duration::from_millis(20);

/// 单次读缓冲。终端输出的突发（`cat` 大文件）会远超这个值，
/// 靠 chunk 分片喂给前端反而更顺，不必在此放大。
const READ_CHUNK: usize = 8 * 1024;

/// 「我们主动终止」之后等子进程咽气的时间，以及 SIGHUP 无效后再强杀的同样的时间。
/// 两段的量级都由「人关掉一个标签页后愿意为主进程退出等多久」决定，而不是由
/// 子进程的实际退出速度决定：正常 shell 收到 SIGHUP 是毫秒级的事，2 秒纯属富余，
/// 真正吃满这两段的只有「改了信号处置、赖着不死」的进程——那种情况下继续等毫无意义。
const KILL_GRACE: Duration = Duration::from_secs(2);

/// 会话向外界吐的事件。
#[derive(Debug)]
pub enum PtyEvent {
    /// 子进程写到终端的原始字节（PTY 下 stdout/stderr 本来就合流，不分类）
    Data(Vec<u8>),
    /// 子进程退出。读端与退出探测是两个任务，所以 `Data` 有可能排在它后面，
    /// 上层要按「取到 None 才算流结束」来写，别把 Exited 当成终止哨兵。
    Exited(PtyExit),
}

/// 退出原因。
///
/// `signal` 有值时 `code` 一定是 `None`：portable-pty 在信号终止的分支里仍把
/// `exit_code()` 填成 1（库内写死的占位），照抄会谎报「正常退出码 1」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyExit {
    pub code: Option<u32>,
    pub signal: Option<String>,
}

impl PtyExit {
    pub fn success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }

    fn from_status(status: portable_pty::ExitStatus) -> Self {
        let signal = status.signal().map(str::to_owned);
        Self { code: if signal.is_some() { None } else { Some(status.exit_code()) }, signal }
    }
}

#[derive(Debug)]
enum OwnerCmd {
    Resize(portable_pty::PtySize),
    /// 主动终止子进程（用户点了关闭标签页，或会话层被回收）
    Stop,
}

/// 一个运行中的本地终端。
///
/// 只是「控制端 + 事件流」的组合：控制逻辑全在 [`LocalHandle`]，
/// 想要分开持有两半（会话管理器的做法）就用 `split`。
pub struct LocalSession {
    handle: LocalHandle,
    events_rx: UnboundedReceiver<PtyEvent>,
}

impl LocalSession {
    /// 开 pty 并起子进程。
    ///
    /// 必须在 tokio 运行时上下文中调用（内部用 `spawn_blocking`）。
    pub fn spawn(spec: &LocalSpec) -> Result<Self, PtyError> {
        let (cmd, size) = spec.build()?;

        let system = native_pty_system();
        let pair = system
            .openpty(size)
            .map_err(|e| log_other("openpty", e, PtyError::Open))?;
        let master = pair.master;
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| log_other("spawn", e, PtyError::Spawn { program: program_of(spec) }))?;

        let reader = master
            .try_clone_reader()
            .map_err(|e| log_other("clone_reader", e, PtyError::Open))?;
        let writer = master
            .take_writer()
            .map_err(|e| log_other("take_writer", e, PtyError::Open))?;
        let killer = child.clone_killer();

        let (events_tx, events_rx) = unbounded_channel();
        let (stdin_tx, stdin_rx) = sync_channel(STDIN_QUEUE);
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let alive = Arc::new(AtomicBool::new(true));

        spawn_reader(reader, events_tx.clone(), stdin_tx.clone());
        spawn_writer(writer, stdin_rx, alive.clone());
        spawn_owner(master, child, killer, cmd_rx, events_tx, alive.clone());

        Ok(Self { handle: LocalHandle { stdin_tx, cmd_tx, alive }, events_rx })
    }

    /// 送按键/粘贴的原始字节。不做任何换行翻译——xterm.js 发什么就转什么。
    pub fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        self.handle.write(bytes)
    }

    /// 尺寸变化只投递，不等待：拖动分屏时每帧都会调一次，
    /// 等 owner 应答会把 UI 卡住。命令队列不设上限，因为丢掉最后一条
    /// 尺寸会让终端永远停在拖动过程中的某个值。
    pub fn resize(&self, pty: &PtySize) -> Result<(), PtyError> {
        self.handle.resize(pty)
    }

    /// 终止子进程。事件流会在退出后自然结束。
    pub fn kill(&self) {
        self.handle.kill();
    }

    /// 取下一条事件；`None` 表示输出结束且子进程已回收。
    pub async fn next_event(&mut self) -> Option<PtyEvent> {
        self.events_rx.recv().await
    }

    /// 拆成可共享的控制端与独占的事件流。
    ///
    /// 会话管理器需要「一个任务独占事件流做转发，别处还能继续按键/改尺寸」，
    /// 而 `next_event` 要 `&mut self`——单个所有者的借用检查放不下。
    /// 这与 `dbhx_ssh::Shell::into_split` 是同一个形状，不是巧合。
    pub fn split(self) -> (LocalHandle, UnboundedReceiver<PtyEvent>) {
        let Self { handle, events_rx } = self;
        (handle, events_rx)
    }
}

/// 本地终端的控制侧：按键、改尺寸、终止。可以复制、可以多处持有。
#[derive(Clone)]
pub struct LocalHandle {
    stdin_tx: SyncSender<Vec<u8>>,
    cmd_tx: UnboundedSender<OwnerCmd>,
    alive: Arc<AtomicBool>,
}

impl LocalHandle {
    /// 与 `LocalSession::write` 同义。
    pub fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        // 先查存活标志：子进程已经没了的时候，往一条死管道里塞字节只会让
        // 第一次写「成功」、之后才报错，上层没法据此判断该不该停发。
        if !self.alive.load(Ordering::Relaxed) {
            return Err(PtyError::Closed);
        }
        self.stdin_tx.try_send(bytes.to_vec()).map_err(|e| match e {
            TrySendError::Full(_) => PtyError::Busy,
            TrySendError::Disconnected(_) => PtyError::Closed,
        })
    }

    pub fn resize(&self, pty: &PtySize) -> Result<(), PtyError> {
        self.cmd_tx.send(OwnerCmd::Resize(to_wire_size(pty))).map_err(|_| PtyError::Closed)
    }

    pub fn kill(&self) {
        let _ = self.cmd_tx.send(OwnerCmd::Stop);
    }

    /// 子进程是否还在。退出探测与这里是异步的，只用于「尽早拒绝按键」，不是精确信号。
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

fn program_of(spec: &LocalSpec) -> String {
    spec.program.clone().unwrap_or_else(|| "<默认 shell>".to_string())
}

/// portable-pty 的失败原文（OS 报错、ConPTY 细节）只留在日志里，对外只给分类。
/// 形参用 Debug 而不是具体的 anyhow::Error：anyhow 的 Debug 会连 cause 链一起打，
/// 而我们不必为此把 anyhow 变成直接依赖。
fn log_other<T: std::fmt::Debug, E>(stage: &str, err: T, mapped: E) -> E {
    tracing::debug!(stage, error = ?err, "portable-pty 调用失败");
    mapped
}

/// ConPTY 起来后的第一件事是发 `CSI 6 n`（DSR：问光标在哪），并且**在收到
/// `CSI <row>;<col> R` 之前把子进程的输出整段扣住**。实测不回答就只能看到这一条
/// 查询，cmd/powershell 的首屏永远不出现。真终端由仿真器回答，但这一层不能赌
/// webview 里的 xterm 先答，所以读端看到就代答一次。
/// 坐标只能报 1;1：我们不掌握屏幕上的真实光标，而 ConPTY 也只用它确认终端会答。
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_REPLY: &[u8] = b"\x1b[1;1R";

/// 在字节流里找 DSR 查询。查询可能被 `read` 的边界劈开（上一块以 `ESC[6` 结尾），
/// 所以留至多 3 字节的尾巴和下一块开头拼起来再看一次；尾巴本身不足 4 字节，
/// 不可能把已经答过的查询重新拼出来，所以不会重复代答。
struct DsrScanner {
    tail: [u8; DSR_QUERY_LEN_MINUS_1],
    len: usize,
}

const DSR_QUERY_LEN_MINUS_1: usize = DSR_QUERY.len() - 1;

impl DsrScanner {
    fn new() -> Self {
        Self { tail: [0; DSR_QUERY_LEN_MINUS_1], len: 0 }
    }

    fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut hit = contains(DSR_QUERY, chunk);
        if !hit && self.len > 0 {
            let head_len = chunk.len().min(DSR_QUERY_LEN_MINUS_1);
            let mut joined = [0_u8; DSR_QUERY_LEN_MINUS_1 * 2];
            joined[..self.len].copy_from_slice(&self.tail[..self.len]);
            joined[self.len..self.len + head_len].copy_from_slice(&chunk[..head_len]);
            hit = contains(DSR_QUERY, &joined[..self.len + head_len]);
        }
        self.len = chunk.len().min(DSR_QUERY_LEN_MINUS_1);
        self.tail[..self.len].copy_from_slice(&chunk[chunk.len() - self.len..]);
        hit
    }
}

fn contains(needle: &[u8], hay: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: UnboundedSender<PtyEvent>,
    stdin_tx: SyncSender<Vec<u8>>,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0_u8; READ_CHUNK];
        let mut scanner = DsrScanner::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    if scanner.feed(chunk) {
                        // 走正常输入通道，等价于「终端敲了这几个字节」；
                        // 队列满或会话已死时丢掉即可，ConPTY 会重发查询。
                        let _ = stdin_tx.try_send(DSR_REPLY.to_vec());
                    }
                    if tx.send(PtyEvent::Data(chunk.to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    // Windows 上子进程退出后 ConPTY 的读端多半报 BrokenPipe 而不是返回 0，
                    // 两端语义不同但结论一致：没有更多输出了。
                    tracing::debug!(error = %e, "pty 读取结束");
                    break;
                }
            }
        }
    });
}

fn spawn_writer(mut writer: Box<dyn Write + Send>, stdin_rx: Receiver<Vec<u8>>, alive: Arc<AtomicBool>) {
    tokio::task::spawn_blocking(move || {
        loop {
            match stdin_rx.recv_timeout(IDLE_POLL) {
                // Disconnected = LocalSession 被 drop，此处丢掉的 writer 就是给子进程的 EOF。
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    // 子进程已经没了也必须主动收手：ConPTY 的输出管道要等我们这边的
                    // 写句柄全部关闭才会给出 EOF，一直 parked 在这里就会让 reader 任务
                    // 永久占住一个阻塞线程，事件流也永远不结束。
                    if !alive.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Ok(bytes) => {
                    if let Err(e) = writer.write_all(&bytes) {
                        tracing::debug!(error = %e, "pty 写入失败，停止转发输入");
                        break;
                    }
                    let _ = writer.flush();
                }
            }
        }
    });
}

fn spawn_owner(
    master: Box<dyn MasterPty + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    mut killer: Box<dyn ChildKiller + Send + Sync>,
    mut cmd_rx: UnboundedReceiver<OwnerCmd>,
    tx: UnboundedSender<PtyEvent>,
    alive: Arc<AtomicBool>,
) {
    tokio::task::spawn_blocking(move || {
        let mut exit = None;
        loop {
            // 这里不能用 tokio 的异步 recv：任务跑在阻塞线程上，没有 executor 可让。
            // try_recv + 短睡等价于 recv_timeout，而且顺手把队列里积压的尺寸变化一次吃完。
            match cmd_rx.try_recv() {
                Ok(OwnerCmd::Resize(size)) => {
                    if let Err(e) = master.resize(size) {
                        tracing::debug!(error = %e, "pty resize 失败");
                    }
                    continue;
                }
                // 上层显式 kill，或会话句柄全部 drop（用户关了标签页）：都是「我们主动终止」。
                // shell 收到 SIGHUP 后会正常退出并给出退出码；上层若要区分自己发起的终止，
                // 记住自己调用过 kill 即可，不必在这里造第三种状态。
                Ok(OwnerCmd::Stop) | Err(TryRecvError::Disconnected) => {
                    let _ = killer.kill();
                    exit = reap_after_stop(child.as_mut());
                    break;
                }
                Err(TryRecvError::Empty) => std::thread::sleep(IDLE_POLL),
            }
            match child.try_wait() {
                Ok(Some(s)) => {
                    exit = Some(PtyExit::from_status(s));
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "pty 子进程状态查询失败");
                    break;
                }
            }
        }
        // 循环退出即意味着这个会话没救了：先落旗，让 writer 任务和 write() 立刻知道
        alive.store(false, Ordering::Relaxed);
        if let Some(s) = exit {
            let _ = tx.send(PtyEvent::Exited(s));
        }
        // 离开作用域时 drop master：这会让 reader 收到 EOF，事件流随之结束
    });
}

/// 主动终止后的回收。**不能直接用 `Child::wait()`**：portable-pty 在 Unix 上给的
/// killer（`ProcessSignaller`）只发一发 SIGHUP 就返回 Ok，既不保证进程死了、
/// 也不替我们收尸。子进程要是改了 SIGHUP 的处置（`less`、`vim`、任何显式
/// `trap '' HUP` 的脚本都会），紧随其后的无界 `wait` 就永久占死一个阻塞线程；
/// 而 tokio 运行时的 drop 要等阻塞线程池清完，于是「关掉一个本地终端」会升级成
/// 「整个进程再也不退出」——macOS 上 `cargo test` 挂满 CI 就是这么来的。
///
/// 所以这里自己掐表：宽限期内轮询 `try_wait`，到期升级成 SIGKILL 再等一轮，
/// 还等不到就放弃。返回 `None` 的语义与「查询出错」那条分支一致——事件流照常结束，
/// 只是不给退出码，绝不编造一个。
fn reap_after_stop(child: &mut (dyn Child + Send + Sync)) -> Option<PtyExit> {
    match wait_within(child, KILL_GRACE) {
        Waited::Exited(status) => Some(PtyExit::from_status(status)),
        Waited::Timeout => {
            force_kill(child);
            match wait_within(child, KILL_GRACE) {
                Waited::Exited(status) => Some(PtyExit::from_status(status)),
                Waited::Timeout => {
                    tracing::warn!("子进程强杀后仍未在预算内回收，放弃等待");
                    None
                }
                Waited::Failed => None,
            }
        }
        Waited::Failed => None,
    }
}

enum Waited {
    Exited(portable_pty::ExitStatus),
    Timeout,
    Failed,
}

fn wait_within(child: &mut (dyn Child + Send + Sync), budget: Duration) -> Waited {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Waited::Exited(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Waited::Timeout;
                }
                std::thread::sleep(IDLE_POLL);
            }
            Err(e) => {
                tracing::debug!(error = %e, "pty 子进程状态查询失败");
                return Waited::Failed;
            }
        }
    }
}

/// 最后一招。portable-pty 用 `setsid()` 起子进程，它自己就是组长，所以按 pid 发
/// SIGKILL 即可；shell 起的子命令由随后 drop master 时内核对前台进程组补的那发
/// SIGHUP 收尾，不归我们管——我们只保证「等在自己面前的这个进程」一定能死。
///
/// 本 crate 里唯一需要越过 `unsafe_code = "deny"` 的地方：std 的 `Child::kill`
/// 语义就是这个，但它被 `dyn Child` 抹掉了，只能自己发信号。
#[cfg(unix)]
#[allow(unsafe_code)]
fn force_kill(child: &mut (dyn Child + Send + Sync)) {
    let Some(pid) = child.process_id() else { return };
    // SAFETY: kill(2) 只要一个合法的 pid；pid 来自我们自己持有的子进程，
    // 进程表里没有这个 pid 时它返回 ESRCH，不会误伤别的内存。
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH = 已经死了（只是还没收尸），是正常情况；其余才是真出事。
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::debug!(error = %err, "pty 子进程强杀失败");
        }
    }
}

/// Windows 上 `killer.kill()` 走的是 `TerminateProcess`，本来就是立即且强制的，
/// 到不了这里；到了也只能干等，保留同一套预算语义。
#[cfg(windows)]
fn force_kill(_child: &mut (dyn Child + Send + Sync)) {}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;

    /// 真起进程的测试全部带预算：pty 的输出依赖内核调度，
    /// 没有超时的话一旦回归就是 CI 挂死而不是报错。
    const BUDGET: Duration = Duration::from_secs(20);

    /// 一定会退出、且把标记打到终端上的命令。
    #[cfg(windows)]
    fn echo_spec(marker: &str) -> LocalSpec {
        LocalSpec::default().program("cmd.exe").args(&["/C", &format!("echo {marker}")])
    }

    #[cfg(not(windows))]
    fn echo_spec(marker: &str) -> LocalSpec {
        LocalSpec::default().program("/bin/echo").args(&[marker])
    }

    /// 不起交互命令、等着被杀的 shell
    #[cfg(windows)]
    fn shell_spec() -> LocalSpec {
        LocalSpec::default().program("cmd.exe")
    }

    #[cfg(not(windows))]
    fn shell_spec() -> LocalSpec {
        LocalSpec::default().program("/bin/sh")
    }

    /// 用进程自己报告的终端尺寸来证明 PtySize 真的落到了内核上。
    /// Unix 的 `stty size` 输出「行 列」；Windows 用 `mode`，它的标签会随系统
    /// 语言变化，所以只断言数字。
    #[cfg(windows)]
    fn size_probe_spec(pty: PtySize) -> LocalSpec {
        LocalSpec::default().program("cmd.exe").args(&["/C", "mode"]).pty(pty)
    }

    #[cfg(not(windows))]
    fn size_probe_spec(pty: PtySize) -> LocalSpec {
        LocalSpec::default().program("/bin/sh").args(&["-c", "stty size"]).pty(pty)
    }

    #[cfg(windows)]
    fn size_reported(text: &str, pty: &PtySize) -> bool {
        text.contains(&pty.cols.to_string()) && text.contains(&pty.rows.to_string())
    }

    #[cfg(not(windows))]
    fn size_reported(text: &str, pty: &PtySize) -> bool {
        text.contains(&format!("{} {}", pty.rows, pty.cols))
    }

    /// 读到事件流自然结束（两个发送端都 drop），返回全部文本与退出信息。
    async fn drain(session: &mut LocalSession) -> (String, Option<PtyExit>) {
        let mut text = String::new();
        let mut exit = None;
        loop {
            match timeout(BUDGET, session.next_event()).await {
                Err(_elapsed) => panic!("{BUDGET:?} 内事件流没有结束"),
                Ok(None) => break,
                Ok(Some(PtyEvent::Data(bytes))) => text.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(Some(PtyEvent::Exited(s))) => exit = Some(s),
            }
        }
        (text, exit)
    }

    #[tokio::test]
    async fn output_comes_back_as_bytes_and_the_exit_is_reported() {
        let mut session = LocalSession::spawn(&echo_spec("dbhx-pty-marker")).unwrap();
        let (text, exit) = drain(&mut session).await;
        assert!(text.contains("dbhx-pty-marker"), "没有从 pty 读回标记，实际输出: {text:?}");
        assert_eq!(exit, Some(PtyExit { code: Some(0), signal: None }));
    }

    #[tokio::test]
    async fn the_pty_is_created_at_the_requested_size() {
        let pty = PtySize { rows: 40, cols: 120 };
        let mut session = LocalSession::spawn(&size_probe_spec(pty.clone())).unwrap();
        let (text, _) = drain(&mut session).await;
        assert!(size_reported(&text, &pty), "进程看到的尺寸不是 {pty:?}，输出: {text:?}");
    }

    #[tokio::test]
    async fn typed_bytes_are_echoed_back_by_the_terminal() {
        // 不额外断言回显开关：pty 默认 ECHO，写进去的按键会原样出现在输出流里，
        // 这条测试要保护的正是「write 与 read 两条通道都通」。
        let mut session = LocalSession::spawn(&shell_spec()).unwrap();
        let marker = "dbhx-echo-probe";
        session.write(format!("{marker}\r").as_bytes()).unwrap();

        let mut text = String::new();
        for _ in 0..100 {
            if let Ok(Some(PtyEvent::Data(bytes))) =
                timeout(Duration::from_millis(200), session.next_event()).await
            {
                text.push_str(&String::from_utf8_lossy(&bytes));
                if text.contains(marker) {
                    break;
                }
            }
        }
        session.kill();
        assert!(text.contains(marker), "按键没有回显，实际输出: {text:?}");
    }

    #[tokio::test]
    async fn kill_produces_an_exit_event_and_closes_the_stream() {
        let mut session = LocalSession::spawn(&shell_spec()).unwrap();
        session.kill();
        let (_, exit) = drain(&mut session).await;
        assert!(exit.is_some(), "kill 之后必须给出退出事件，否则 UI 不知道标签页可以回收");
    }

    /// 对 SIGHUP 免疫的子进程也必须收尾。portable-pty 在 Unix 上的 killer 只发一发
    /// SIGHUP 就报 Ok，早先这里紧跟一个无界 `Child::wait`：这种进程不把信号当回事，
    /// owner 线程就永久占死，运行时 drop 再等阻塞线程池——整个测试进程再也不退出。
    /// 用 `read` 而不是 `sleep`：不派孙进程，pty 从句柄不会在别人手里多留 20 秒。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_that_ignores_sighup_is_force_reaped() {
        let spec = LocalSpec::default().program("/bin/sh").args(&["-c", "trap '' HUP; read -r line"]);
        let mut session = LocalSession::spawn(&spec).unwrap();
        session.kill();
        let (_, exit) = drain(&mut session).await;
        let exit = exit.expect("强杀之后仍要给出退出事件");
        // 只断「被信号打死」：信号名走的是 strsignal，随系统语言变，不该拿来断言。
        assert!(exit.signal.is_some() && exit.code.is_none(), "不是信号终止：{exit:?}");
    }

    #[tokio::test]
    async fn writing_to_a_finished_session_is_an_error_not_a_panic() {
        let mut session = LocalSession::spawn(&echo_spec("dbhx-done")).unwrap();
        drain(&mut session).await;
        assert!(matches!(session.write(b"x"), Err(PtyError::Closed)));
        assert!(matches!(session.resize(&PtySize::default()), Err(PtyError::Closed)));
    }

    #[tokio::test]
    async fn split_leaves_input_on_the_handle_and_exit_on_the_stream() {
        // 会话管理器就是把两半分开持有的，这条测试守的是「分开之后两边都还活着」
        let session = LocalSession::spawn(&shell_spec()).unwrap();
        session.kill();
        let (handle, mut events) = session.split();
        loop {
            match timeout(BUDGET, events.recv()).await {
                Err(_elapsed) => panic!("{BUDGET:?} 内事件流没有结束"),
                Ok(None) | Ok(Some(PtyEvent::Exited(_))) => break,
                Ok(Some(PtyEvent::Data(_))) => {}
            }
        }
        // owner 是「先落存活标志、再发退出事件」，所以流结束之后此处必须是确定的
        assert!(!handle.alive(), "kill 之后存活标志要立刻可见");
        assert!(matches!(handle.write(b"x"), Err(PtyError::Closed)));
    }

    #[tokio::test]
    async fn a_missing_program_fails_before_any_thread_is_spawned() {
        let spec = LocalSpec::default().program("dbhx-not-installed-anywhere");
        assert!(matches!(
            LocalSession::spawn(&spec),
            Err(PtyError::Spawn { program: _ })
        ));
    }

    #[test]
    fn dsr_query_is_detected_whole_or_split() {
        let mut scanner = DsrScanner::new();
        assert!(scanner.feed(b"\x1b[6n"), "整块里的查询必须认出来");

        let mut split = DsrScanner::new();
        assert!(!split.feed(b"title\x1b[6"), "半个查询不该触发回答");
        assert!(split.feed(b"n\x1b[?25h"), "劈成两半的查询也要认出来");

        let mut again = DsrScanner::new();
        assert!(again.feed(b"\x1b[6n"));
        assert!(!again.feed(b"\x1b[?25h"), "答过的查询不能被尾巴重新拼出来再答一次");
    }
}
