//! 通道：shell / exec / subsystem 的建、确认、收发。
//!
//! russh 在这里有个**反直觉的坑**：`request_pty` / `request_shell` / `exec` /
//! `request_subsystem` 只要消息发得出去就返回 `Ok(())`，服务器稍后拒绝也不会
//! 让这个返回值变红。所以每个 `want_reply: true` 的请求之后都必须自己
//! `wait()` 到 `ChannelMsg::Success`，否则拿到的是一个「看起来成功了」的死通道。
//!
//! 读写半区刻意拆开（`split`），这样终端层可以把读循环放进独立任务，
//! 写路径不再碰任何共享锁——键盘回显对延迟敏感，值得这个复杂度。

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
        match self.read.wait().await? {
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
                tracing::debug!(channel = %self.id, ?other, "忽略未预期的通道消息");
                Some(Chunk::Control(other))
            }
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
pub struct Shell(pub SplitChannel);

impl Shell {
    pub async fn write(&self, bytes: &[u8]) -> Result<(), SshError> {
        self.0.write(bytes).await
    }

    pub async fn resize(&self, pty: &PtySize) -> Result<(), SshError> {
        self.0.resize(pty).await
    }

    pub async fn close(&self) -> Result<(), SshError> {
        self.0.close().await
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

/// 等 `Success`。`Failure` / `Close` / 超时都翻译成可区分的错误，
/// 让上层能分辨「服务器不支持这个 subsystem」和「网络断了」。
pub(crate) async fn confirm(
    channel: &mut WireChannel,
    request: &'static str,
) -> Result<(), SshError> {
    match timeout(REQUEST_TIMEOUT, channel.wait()).await {
        Err(_elapsed) => Err(SshError::Timeout),
        Ok(None) => Err(SshError::Closed),
        Ok(Some(ChannelMsg::Success)) => Ok(()),
        Ok(Some(ChannelMsg::Failure)) | Ok(Some(ChannelMsg::Close)) => {
            Err(SshError::refused(request))
        }
        Ok(Some(other)) => {
            // 服务器先塞数据再回 Success 的可能性极低，但真遇到要能诊断
            tracing::debug!(?other, request, "等待通道确认时收到其它消息");
            Err(SshError::refused(request))
        }
    }
}
