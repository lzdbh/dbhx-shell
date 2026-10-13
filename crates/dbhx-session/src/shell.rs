//! 远端交互 shell：通道字节流 ⇒ 前端事件。
//!
//! 会话层在这里只做三件事，每件都对应参考实现里踩过的坑：
//!
//! 1. **字节不解释**。终端仿真交给前端的 xterm，会话层只搬运。任何「顺手过滤一下转义序列」
//!    的冲动都会让 vim/top 花屏，且极难复现。唯一的例外是 OSC 7（见 [`crate::cwd`]），
//!    它只旁路读出一个目录，送出去的字节一个不少。
//! 2. **exit-status 不等于会话结束**。sshd 常常先发 `exit-status` 再发最后几行
//!    （`logout`、`Connection to x closed`），立刻收尾就会吞掉它们。所以退出码先攒着，
//!    流真正结束才发 [`ShellEvent::Ended`]。
//! 3. **一条通道只有一个读者**。读端独占并交给读任务，写端是可共享的 `ShellWriter`——
//!    两半各归各处正是 `Shell::into_split` 存在的理由。

use std::sync::Arc;

use dbhx_model::{LiveEndpoint, PtySize, SessionId};
use dbhx_ssh::{Chunk, Shell, ShellReader, ShellWriter};
use tokio::sync::{mpsc, watch, Mutex};

use crate::cwd::CwdTracker;
use crate::error::SessionError;
use crate::pool::Lease;

/// 事件队列长度。输出是流式的，正常消费不该积压；满了说明前端已经不读了
/// （webview 被挂起），此时让读任务阻塞、进而让 sshd 的窗口停摆，比无脑堆内存好。
const EVENT_QUEUE: usize = 512;

/// shell 会话对外吐的事件。
#[derive(Debug, Clone)]
pub enum ShellEvent {
    /// 远端终端的原始输出字节
    Data(Arc<[u8]>),
    /// shell 通过 OSC 7 报告的当前目录。SFTP 面板据此跟随 `cd`。
    Cwd(String),
    /// 会话结束。`exit_status` 与 `signal` 可能都是 `None`——通道被直接关掉、
    /// 或整条连接断掉时就是这个形态。
    Ended(ExitInfo),
}

/// 结束原因。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExitInfo {
    pub exit_status: Option<u32>,
    pub signal: Option<String>,
}

impl ExitInfo {
    /// shell 正常退出（`exit` 打完招呼就走了）。
    pub fn clean(&self) -> bool {
        self.exit_status == Some(0) && self.signal.is_none()
    }
}

/// 通道消息 ⇒ 事件的纯函数状态机。
///
/// 之所以独立成类型：读任务的壳（`while let Some(chunk) = reader.next_chunk().await`）
/// 没有分支可言，真正会写错的只有这张映射表——它现在可以在毫秒级单测里逐条消息验证，
/// 不需要真连接。
#[derive(Debug, Default)]
struct Pump {
    cwd: CwdTracker,
    exit: ExitInfo,
}

impl Pump {
    /// 处理一条通道消息。返回要按序发出去的事件，通常 0～2 条。
    fn push(&mut self, chunk: Option<Chunk>) -> Vec<ShellEvent> {
        match chunk {
            // 流结束：此时才宣告会话可以回收
            None => vec![ShellEvent::Ended(std::mem::take(&mut self.exit))],
            Some(Chunk::Stdout(bytes)) | Some(Chunk::Stderr(bytes)) => self.data(&bytes),
            Some(Chunk::Other { ext, data }) => {
                tracing::debug!(ext, "交互通道上的扩展数据按输出处理");
                self.data(&data)
            }
            Some(Chunk::ExitStatus(code)) => {
                self.exit.exit_status = Some(code);
                Vec::new()
            }
            Some(Chunk::ExitSignal(signal)) => {
                self.exit.signal = Some(signal);
                Vec::new()
            }
            // 窗口调整是 sshd 对我们的流控反馈，前端不需要知道
            Some(Chunk::WindowAdjusted { .. }) => Vec::new(),
            Some(Chunk::Control(msg)) => {
                tracing::debug!(?msg, "未预期的通道消息已忽略（未丢弃任何字节）");
                Vec::new()
            }
        }
    }

    fn data(&mut self, bytes: &[u8]) -> Vec<ShellEvent> {
        let mut out = Vec::with_capacity(2);
        if bytes.is_empty() {
            return out;
        }
        out.push(ShellEvent::Data(Arc::from(bytes)));
        // 目录变化排在数据之后：前端先渲染输出，再决定要不要动文件面板
        if let Some(path) = self.cwd.feed(bytes) {
            out.push(ShellEvent::Cwd(path));
        }
        out
    }
}

/// 一个运行中的远端 shell。
///
/// 同时持有 lease 与通道两半：**只要这个对象活着，底下的连接就不会被算作空闲**。
/// 关掉标签页（drop）后连接进入 idle park，而不是立刻断开。
pub struct ShellSession {
    id: SessionId,
    writer: ShellWriter,
    events: Mutex<mpsc::Receiver<ShellEvent>>,
    /// 读任务同步出来的当前目录，供「面板先开、终端后报」这类顺序查询。
    cwd: watch::Receiver<Option<String>>,
    /// 只在 drop 时有意义：归还引用计数
    lease: Lease,
}

impl ShellSession {
    /// 在一条已认证的连接上开 shell 通道并起读任务。
    pub async fn start(id: SessionId, lease: Lease, term: String, pty: PtySize) -> Result<Self, SessionError> {
        let shell = lease.open_shell(term, pty).await?;
        Ok(Self::from_shell(id, lease, shell))
    }

    /// 拆通道、起读任务。与 `start` 分开是为了让「拿通道」这一步在测试里可替换。
    pub fn from_shell(id: SessionId, lease: Lease, shell: Shell) -> Self {
        let (writer, reader) = shell.into_split();
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        let (cwd_tx, cwd) = watch::channel(None);
        spawn_reader(reader, tx, cwd_tx, id);
        Self { id, writer, events: Mutex::new(rx), cwd, lease }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn endpoint(&self) -> &LiveEndpoint {
        self.lease.endpoint()
    }

    /// 当前已知的终端目录。`None` = 服务器没告过，别猜。
    ///
    /// 这个查询存在的意义是：文件面板可能是**先**被打开的那个，
    /// 它要能拿到终端已经报出的目录，而不是只能等下一次 `cd` 的事件。
    pub fn cwd(&self) -> Option<String> {
        self.cwd.borrow().clone()
    }

    /// 键盘输入。原样转发，不做任何换行/退格翻译——那是 xterm.js 的事。
    pub async fn write(&self, bytes: &[u8]) -> Result<(), SessionError> {
        self.writer.write(bytes).await.map_err(SessionError::Ssh)
    }

    /// 尺寸变化。拖动分屏时每帧都会调一次，失败只说明通道没了，不值得打断用户。
    pub async fn resize(&self, pty: &PtySize) -> Result<(), SessionError> {
        self.writer.resize(pty).await.map_err(SessionError::Ssh)
    }

    /// 关闭通道。连接去留由 lease 计数决定，与这里无关。
    pub async fn close(&self) -> Result<(), SessionError> {
        self.writer.close().await.map_err(SessionError::Ssh)
    }

    /// 取下一条事件；`None` 表示事件流已结束。
    ///
    /// 只有一个消费者能拿到（`Mutex` 而不是多播），这是刻意的：输出字节一旦有两个读者
    /// 就会随机丢掉一半，症状是「偶尔花屏」，几乎无法排查。
    pub async fn next_event(&self) -> Option<ShellEvent> {
        self.events.lock().await.recv().await
    }
}

/// 读任务：通道消息交给 [`Pump`]，事件进队列，目录同时落进 `watch`。
///
/// 不保留 `JoinHandle`：结束条件是「通道结束」或「接收端已 drop」，两者都意味着没有下一步
/// 要等；任务持有的 `ShellReader` 与发送端随之消失。
fn spawn_reader(
    mut reader: ShellReader,
    tx: mpsc::Sender<ShellEvent>,
    cwd_tx: watch::Sender<Option<String>>,
    id: SessionId,
) {
    tokio::spawn(async move {
        let mut pump = Pump::default();
        loop {
            let chunk = reader.next_chunk().await;
            // `next_chunk()` 一旦给出 `None` 就永远是 `None`，必须在这里收手，
            // 否则这个循环会变成空转（发完 Ended 又立刻取到 None 再发一遍）。
            let ended = chunk.is_none();
            for event in pump.push(chunk) {
                if let ShellEvent::Cwd(path) = &event {
                    // 目录既要进事件流，也要能被面板事后查到
                    let _ = cwd_tx.send(Some(path.clone()));
                }
                if tx.send(event).await.is_err() {
                    tracing::debug!(?id, "事件接收端已断开，结束读任务");
                    return;
                }
            }
            if ended {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生产代码里 `push` 收到的永远是 `next_chunk()` 的返回，测试也照这个形状写。
    fn text(s: &str) -> Option<Chunk> {
        Some(Chunk::Stdout(s.as_bytes().to_vec()))
    }

    fn msg(chunk: Chunk) -> Option<Chunk> {
        Some(chunk)
    }

    fn osc(payload: &str) -> String {
        format!("\x1b]{payload}\x07")
    }

    #[test]
    fn stdout_is_forwarded_byte_for_byte() {
        let mut pump = Pump::default();
        let events = pump.push(text("hi\x1b[0m"));
        assert_eq!(events.len(), 1);
        match &events[0] {
            ShellEvent::Data(bytes) => assert_eq!(bytes.as_ref(), b"hi\x1b[0m".as_slice()),
            other => panic!("期望原始字节，实际 {other:?}"),
        }
    }

    #[test]
    fn exit_status_alone_does_not_end_the_session() {
        // 本文件最容易写错的一条：退出码之后还有尾巴
        let mut pump = Pump::default();
        assert!(pump.push(msg(Chunk::ExitStatus(0))).is_empty(), "退出码不该立刻收尾");
        assert_eq!(pump.push(text("logout")).len(), 1);
        let ended = pump.push(None);
        assert!(matches!(&ended[0], ShellEvent::Ended(info) if info.exit_status == Some(0)));
    }

    #[test]
    fn stream_end_without_exit_status_is_still_an_end() {
        let mut pump = Pump::default();
        let ended = pump.push(None);
        assert!(matches!(&ended[0], ShellEvent::Ended(info) if *info == ExitInfo::default()));
    }

    #[test]
    fn signal_and_status_are_kept_separate() {
        let mut pump = Pump::default();
        pump.push(msg(Chunk::ExitSignal("KILL".into())));
        let ended = pump.push(None);
        assert!(matches!(
            &ended[0],
            ShellEvent::Ended(info) if info.signal.as_deref() == Some("KILL") && info.exit_status.is_none()
        ));
    }

    #[test]
    fn ended_info_is_consumed_once() {
        // mem::take 保证重复调用 push(None) 不会把同一份退出码报两次
        let mut pump = Pump::default();
        pump.push(msg(Chunk::ExitStatus(3)));
        assert!(matches!(&pump.push(None)[0], ShellEvent::Ended(info) if info.exit_status == Some(3)));
        assert!(matches!(&pump.push(None)[0], ShellEvent::Ended(info) if info.exit_status.is_none()));
    }

    #[test]
    fn cwd_change_follows_the_data_event() {
        let mut pump = Pump::default();
        let events = pump.push(text(&osc("7;file://h/var/log")));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ShellEvent::Data(_)));
        assert!(matches!(&events[1], ShellEvent::Cwd(p) if p == "/var/log"));
    }

    #[test]
    fn cwd_split_across_two_messages_is_reported_once() {
        let mut pump = Pump::default();
        let mut seen = Vec::new();
        for chunk in [text("x\x1b]7;file://h/e"), text("tc\x07")] {
            seen.extend(pump.push(chunk).into_iter().filter(|e| matches!(e, ShellEvent::Cwd(_))));
        }
        assert_eq!(seen.len(), 1, "劈开的序列必须只报一次，否则面板连跳两下");
    }

    #[test]
    fn control_and_flow_messages_produce_no_events_but_keep_bytes() {
        let mut pump = Pump::default();
        assert!(pump.push(msg(Chunk::WindowAdjusted { bytes: 4096 })).is_empty());
        assert_eq!(pump.push(msg(Chunk::Stderr(b"oops".to_vec()))).len(), 1, "扩展错误按输出送，不丢字节");
        assert!(pump.push(msg(Chunk::Stdout(Vec::new()))).is_empty(), "空包不该占一个事件位");
    }
}
