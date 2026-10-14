//! 通道：shell / exec / subsystem 的建、确认、收发。
//!
//! russh 在这里有个**反直觉的坑**：`request_pty` / `request_shell` / `exec` /
//! `request_subsystem` 只要消息发得出去就返回 `Ok(())`，服务器稍后拒绝也不会
//! 让这个返回值变红。所以每个 `want_reply: true` 的请求之后都必须自己
//! `wait()` 到 `ChannelMsg::Success`，否则拿到的是一个「看起来成功了」的死通道。
//!
//! 读写半区刻意拆开（`split`），这样终端层可以把读循环放进独立任务，
//! 写路径不再碰任何共享锁——键盘回显对延迟敏感，值得这个复杂度。

use std::sync::Arc;

use dbhx_model::PtySize;
use russh::{Channel, ChannelReadHalf, ChannelWriteHalf};
use russh::client::Msg;
use russh::{ChannelId, ChannelMsg};
use tokio::time::{timeout, Duration};

use crate::error::SshError;

/// 等一个通道请求的 `Success` 的预算。10 秒比整个握手超时短，因为走到这一步
/// 链路已经通了，卡住的只会是服务器策略。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub type WireChannel = Channel<Msg>;

/// 拆开的通道。字段公开是有意的：终端层需要把两半分别 move 进不同任务。
pub struct SplitChannel {
    pub id: ChannelId,
    pub read: ChannelReadHalf,
    pub write: ChannelWriteHalf<Msg>,
}

impl SplitChannel {
    pub(crate) fn new(channel: WireChannel) -> Self {
        let id = channel.id();
        let (read, write) = channel.split();
        Self { id, read, write }
    }

    pub async fn write(&self, bytes: &[u8]) -> Result<(), SshError> {
        self.write.data_bytes(bytes.to_vec()).await?;
        Ok(())
    }

    /// 终端尺寸变化。russh 的方法名是 `window_change`（不是 `window_changed`）；
    /// 像素宽高传 0 合法，多数服务器只看行列。
    pub async fn resize(&self, pty: &PtySize) -> Result<(), SshError> {
        self.write
            .window_change(u32::from(pty.cols), u32::from(pty.rows), 0, 0)
            .await?;
        Ok(())
    }

    pub async fn send_signal(&self, sig: russh::Sig) -> Result<(), SshError> {
        self.write.signal(sig).await?;
        Ok(())
    }

    /// 发 EOF（告诉远端 stdin 结束），不关闭通道。
    pub async fn close_write(&self) -> Result<(), SshError> {
        self.write.eof().await?;
        Ok(())
    }

    pub async fn close(&self) -> Result<(), SshError> {
        self.write.close().await?;
        Ok(())
    }

    /// 读一条通道消息。`None` 表示通道已结束（EOF 或 close）。
    pub async fn next_msg(&mut self) -> Option<ChannelMsg> {
        self.read.wait().await
    }

    /// 只取数据类消息；`WindowAdjusted` 之类包成 `Control` 交回调用方处置。
    pub async fn read_chunk(&mut self) -> Option<Chunk> {
        chunk_of(self.id, self.read.wait().await?)
    }
}

/// 通道消息 ⇒ 终端层关心的 `Chunk`。读半区与 `ShellReader` 共用这一份映射，
/// 避免两处对「哪些消息算数据」有不同答案。
///
/// 返回 `Option`：`None` 是「通道到此为止」的信号，所以 `Eof`/`Close` 只能在这里一次用完，
/// 调用方拿到 `None` 之后不该再期待任何东西。
fn chunk_of(id: ChannelId, msg: ChannelMsg) -> Option<Chunk> {
    match msg {
        ChannelMsg::Data { data } => Some(Chunk::Stdout(data.to_vec())),
        ChannelMsg::ExtendedData { ext, data } => Some(if ext == 1 {
            Chunk::Stderr(data.to_vec())
        } else {
            Chunk::Other { ext, data: data.to_vec() }
        }),
        ChannelMsg::Eof | ChannelMsg::Close => None,
        ChannelMsg::WindowAdjusted { new_size } => Some(Chunk::WindowAdjusted { bytes: new_size }),
        ChannelMsg::ExitStatus { exit_status } => Some(Chunk::ExitStatus(exit_status)),
        ChannelMsg::ExitSignal { signal_name, .. } => {
            Some(Chunk::ExitSignal(format!("{signal_name:?}")))
        }
        other => {
            // 终端通道不该收到 Success/Open/XonXoff 之类；记下来但不吞掉字节流
            tracing::debug!(channel = %id, ?other, "忽略未预期的通道消息");
            Some(Chunk::Control(other))
        }
    }
}

#[derive(Debug)]
pub enum Chunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// sshd 极少用非 1 的 extended code；留着，但不当错误丢数据
    Other { ext: u32, data: Vec<u8> },
    WindowAdjusted { bytes: u32 },
    ExitStatus(u32),
    ExitSignal(String),
    /// 未预期但无害的消息，交给调用方决定；不静默丢弃
    Control(ChannelMsg),
}

/// 一条已确认（pty + shell 都收到 Success）的交互通道。
///
/// 本身没有方法：它唯一的作用是把自己拆成读写两半（`into_split`）。
/// 之所以不给它 `write`/`read` 方法，是因为终端必须**同时**读和写，
/// 而一个所有者的 `select!` 里读分支要 `&mut`、写分支要 `&`，借用检查一定会拦住。
pub struct Shell(pub SplitChannel);

impl Shell {
    /// 拆成可共享的写端与独占的读端。
    ///
    /// `ChannelWriteHalf` 不是 `Clone`（`ChannelReadHalf` 也不是），所以写端套一层 `Arc`；
    /// 读端保持独占——它的 `wait` 要 `&mut self`，而且一个字节流不该有两个读者。
    pub fn into_split(self) -> (ShellWriter, ShellReader) {
        let Shell(SplitChannel { id, read, write }) = self;
        (ShellWriter { inner: Arc::new(write) }, ShellReader { id, inner: read })
    }
}

/// 终端输入侧：键盘字节、尺寸变化、关闭。可以多处共享。
#[derive(Clone)]
pub struct ShellWriter {
    inner: Arc<ChannelWriteHalf<Msg>>,
}

impl ShellWriter {
    pub fn id(&self) -> ChannelId {
        self.inner.id()
    }

    pub async fn write(&self, bytes: &[u8]) -> Result<(), SshError> {
        self.inner.data_bytes(bytes.to_vec()).await?;
        Ok(())
    }

    pub async fn resize(&self, pty: &PtySize) -> Result<(), SshError> {
        self.inner
            .window_change(u32::from(pty.cols), u32::from(pty.rows), 0, 0)
            .await
            .map_err(Into::into)
    }

    pub async fn send_signal(&self, sig: russh::Sig) -> Result<(), SshError> {
        self.inner.signal(sig).await.map_err(Into::into)
    }

    /// 告诉远端 stdin 结束，不关闭通道。
    pub async fn close_write(&self) -> Result<(), SshError> {
        self.inner.eof().await.map_err(Into::into)
    }

    pub async fn close(&self) -> Result<(), SshError> {
        self.inner.close().await.map_err(Into::into)
    }
}

/// 终端输出侧。独占，由会话层的读循环持有。
pub struct ShellReader {
    id: ChannelId,
    inner: ChannelReadHalf,
}

impl ShellReader {
    pub fn id(&self) -> ChannelId {
        self.id
    }

    /// 下一条通道消息。`None` 表示通道结束（EOF、close 或对端断开）。
    pub async fn next_chunk(&mut self) -> Option<Chunk> {
        chunk_of(self.id, self.inner.wait().await?)
    }
}

/// 一条已确认的 `exec` 通道。
pub struct Exec(pub SplitChannel);

impl Exec {
    pub async fn write(&self, bytes: &[u8]) -> Result<(), SshError> {
        self.0.write(bytes).await
    }

    pub async fn close(&self) -> Result<(), SshError> {
        self.0.close().await
    }

    /// 收完整输出。命令没有交互，直接读到通道结束。
    pub async fn run_to_end(mut self) -> Result<ExecOutput, SshError> {
        let mut out = ExecOutput::default();
        while let Some(msg) = self.0.read.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { ext, data } if ext == 1 => {
                    out.stderr.extend_from_slice(&data)
                }
                ChannelMsg::ExtendedData { data, .. } => out.stdout.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => out.exit_status = Some(exit_status),
                ChannelMsg::ExitSignal { signal_name, .. } => {
                    out.exit_signal = Some(format!("{signal_name:?}"))
                }
                ChannelMsg::OpenFailure(f) => {
                    return Err(SshError::Russh(russh::Error::ChannelOpenFailure(f)))
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Default)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` = 服务器没回 exit-status（通常是通道被直接关掉）
    pub exit_status: Option<u32>,
    /// 被信号杀死时才有；与 `exit_status` 互斥
    pub exit_signal: Option<String>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.exit_status == Some(0) && self.exit_signal.is_none()
    }

    pub fn stdout_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// 等 `Success`。`Failure` / `Close` / 超时翻译成可区分的错误，
/// 让上层能分辨「服务器不支持这个 subsystem」和「网络断了」。
///
/// 必须**循环**而不是只看一条消息：sshd 常在 `want_reply` 的 `Success` 之前先把
/// `WINDOW_ADJUST`（它上调我们的发送窗口，属纯流控、不携带字节）塞进队列。
/// 若像最初那样「收到非 Success 就判定被拒」，就会把正常开着的 shell 误杀——
/// 症状是会话刚建立就 self-teardown、前端显示「连接已关闭」。
/// 因此这里对不携带数据的流控消息（[`ChannelMsg::WindowAdjusted`] 等）继续等，
/// 只有明确的拒绝/关闭/超时才收手；其余异常消息记下来后同样继续，宁可让
/// [`REQUEST_TIMEOUT`] 兜底，也不主动掐断一条活的通道。
pub(crate) async fn confirm(
    channel: &mut WireChannel,
    request: &'static str,
) -> Result<(), SshError> {
    loop {
        match timeout(REQUEST_TIMEOUT, channel.wait()).await {
            Err(_elapsed) => return Err(SshError::Timeout),
            Ok(None) => return Err(SshError::Closed),
            Ok(Some(ChannelMsg::Success)) => return Ok(()),
            Ok(Some(ChannelMsg::Failure)) | Ok(Some(ChannelMsg::Close)) => {
                return Err(SshError::refused(request))
            }
            Ok(Some(ChannelMsg::OpenFailure(f))) => {
                return Err(SshError::Russh(russh::Error::ChannelOpenFailure(f)))
            }
            // 不携带字节的流控消息：跳过，继续等真正的 Success
            Ok(Some(other @ (ChannelMsg::WindowAdjusted { .. } | ChannelMsg::XonXoff { .. }))) => {
                tracing::debug!(?other, request, "通道确认前收到流控消息，继续等待 Success");
            }
            Ok(Some(other)) => {
                // 理论上 Success 先于数据；真遇到数据抢在前面也别急着判死，
                // 记下来继续等——丢了 Success 才真没法区分「被拒」和「卡住」。
                tracing::debug!(?other, request, "等待通道确认时收到其它消息，继续等待 Success");
            }
        }
    }
}
