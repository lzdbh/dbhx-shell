//! 「用当前用户的 sudo 口令提权跑 sftp-server」的通道层。
//!
//! ## 为什么不能走 subsystem
//!
//! SFTP 是 sshd 的 subsystem，子系统的身份在**认证完成那一刻**就定死了：协议里
//! 没有「在已有连接上换个身份再开一次」这条路。想以别的用户看远端文件，唯一干净的
//! 形态是在一条已认证的会话通道里让 sudo 前台起一个 sftp-server，再把这条通道的
//! 字节流当成 SFTP 线用——服务器侧 `sftp(1)` 的 `-S /path/to/sudo sftp-server`
//! 写的正是这个用法，这里等价地在客户端拼出来。
//!
//! ## 就绪靠的是标记，不是 `Success`
//!
//! 这是整条路上最容易搞错的一点：`exec` 的 `Success` 只表示**命令被起起来了**，
//! 和 sudo 后来认不认这次口令没有任何关系（真 sshd 在 fork 完子进程就发确认）。
//! 拿 `Success` 当「提权成功」用，得到的是一个已经交给 SFTP 解析器的、里面其实是
//! `sudo: … is not in the sudoers file` 的字节流，症状是一句谁也看不懂的协议错误，
//! 而且违背了本项目「失败必须说清楚，不许静默降级」的规矩。
//!
//! 所以命令串里安排了一句 `echo`，而它**在 sudo 之后**执行：
//! `sudo -u USER sh -c 'echo DBHX-ELEVATE-OK; exec "$0"' "$srv"`。能打印出这行标记，
//! 就说明认证过了、sudoers 也放行了、目标用户下的 shell 真的起来了。标记之前到达的
//! 一切字节都是远端在喊话，按模式翻成中文抛出去；标记之后才是 SFTP 线。
//!
//! ## 这条字节流为什么敢当 SFTP 线用
//!
//! * **口令不进命令串**。它一旦进了 argv 就会出现在远端 `ps`/`w` 里，本地也多一份
//!   不被 zeroize 的副本。命令串里只有 `IFS= read -r pw`：口令由通道的第一个字节行
//!   交进来，落到 600 权限的临时文件，变量随即清空。
//! * **密码提示不占字节流**。`sudo -A` 走 `SUDO_ASKPASS`，askpass 的约定是「把自己的
//!   答案写到 stdout」（`sudo_askpass(1)` 的 MANPAGE 注意事项）。tty 的 stdin 自始至终
//!   只归 sftp-server 一个读者，也不会像 `sudo -S` 配管道那样把服务端的 stdin 换成一条
//!   已经耗尽的管道。
//! * **NOPASSWD 不串味**。免密机器上 askpass 根本不被调用，而口令行已经被前面的
//!   `read` 吃掉了，不会有半行口令混进 SFTP 线。
//! * **标记之后立刻 exec**。`echo` 完就是 `exec "$0"`，同一进程换成 sftp-server，
//!   中间没有任何东西能往这条线上写字——sftp-server 是纯请求驱动的，收到客户端第一个
//!   报文之前一个字节都不写。真有别的字节冒出来就是协议异常，当场报脏而不是当成数据。
//! * **不给 sftp-server 传 `-e`**。它的日志走 stderr，而 pty 通道里 stderr 和 stdout
//!   是同一条线；一旦开了日志，报文流里就会混进纯文本。
//! * **交给 sudo 之前把 tty 切成 raw**。SFTP 报文里处处是 `\n` 与 Ctrl-Q/Ctrl-S，
//!   `ONLCR`/`ISIG`/`IXON` 不关掉线就毁了。口令读进来之前先来一句 `stty -echo`，
//!   免得那几十字节被 tty 回显进日志（真回显了也有兜底，见 `redacted`）。
//!
//! 远端 sudo 不给跑是常态而不是意外——sudoers 只放行少数命令的环境非常多。所以这里
//! 按模式把远端原话翻成可行动的中文，原文只进 `tracing::debug` 且先抹掉口令。

use std::borrow::Cow;
use std::time::Duration;

use dbhx_model::LiveEndpoint;
use russh::client::Msg;
use russh::{Channel, ChannelMsg, ChannelStream};
use tokio::time::timeout;
use zeroize::Zeroizing;

use crate::error::SshError;

/// 提权等待预算。sudo 认证加起 sftp-server 在正常机器上是亚秒级；15 秒既要盖得住
/// 慢速 PAM（LDAP/AD 后端），也要让「远端压根不响应」尽快定性成超时。
pub const ELEVATE_TIMEOUT: Duration = Duration::from_secs(15);

/// 口令行长上限。tty 的 canonical 行缓冲多数是 4096 字节，超长会被悄悄截断，
/// 症状是「口令明明对却认证失败」——那不如在这里就说清楚。
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// 就绪标记。由**提权后的** shell 打印，见模块注释。
///
/// 长度固定、纯 ASCII、不与任何 SFTP 报文的前缀重样（报文头第一个字节是 4 字节
/// 长度的首字节，取值范围只有 0x00–0x7F 这一小截），所以拿它做子串搜索不会误判。
const READY_MARKER_TEXT: &str = "DBHX-ELEVATE-OK";
const READY_MARKER: &[u8] = READY_MARKER_TEXT.as_bytes();

/// 提权通道上远端 shell 看到的终端类型。`dumb` 让 shell 不往线里塞 ANSI 序列；
/// 行列只影响 `stty` 的显示宽度，取默认值即可。
const ELEVATE_TERM: &str = "dumb";
const ELEVATE_COLS: u32 = 80;
const ELEVATE_ROWS: u32 = 24;

/// sftp-server 的候选路径，覆盖 Debian/Ubuntu、RHEL 系与 SUSE。
/// 一个都不在时仍按第一条去跑：让远端自己报「找不到」，比本地猜成功诚实。
const SFTP_SERVER_PROBE: &str = "srv=/usr/lib/openssh/sftp-server; \
     for c in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server \
       /usr/libexec/sftp-server /usr/lib/sftp-server; \
     do [ -x \"$c\" ] && srv=\"$c\" && break; done; ";

/// 把参数包进单引号，内部单引号换成 shell 惯写法 `'\''`。
///
/// 提权用户名来自前端输入，命令串里它只有这一个入口，没有第二条路能绕过这个函数。
pub fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

/// 组装 exec 的命令串。一行写完，不留 here-doc：正文里出现换行会让远端 shell 把
/// 「读口令那一行」和脚本本身混在同一条 stdin 上，边界一糊就是认证失败。
///
/// `sudo -k` 拒绝复用缓存凭据：用户点一次「提权」就是一次新的授权，不该被几小时前
/// 那次输入蒙过去。`rm -rf "$d"` 在两条路上都要走（askpass 里一次、退出前一次），
/// 因为免密机器上 askpass 压根不运行。
///
/// `sh -c '…'` 的那两个单引号是**命令串自己的**，不参与 `shell_quote`：里面没有展开，
/// `$0` 由外层把 `$srv` 作为位置参数递进去，所以路径里带空格也不会散架，
/// 而提权用户名永远只在 `-u` 那一个已被引号包住的位置上。
pub fn build_exec_command(sudo_user: &str) -> String {
    let user = shell_quote(sudo_user);
    format!(
        "IFS= read -r pw; \
         d=$(mktemp -d) || {{ echo 'dbhx-elevate: cannot create temp dir'; exit 78; }}; \
         chmod 700 \"$d\"; \
         stty -echo 2>/dev/null; \
         printf %s \"$pw\" > \"$d/pw\"; pw=; chmod 600 \"$d/pw\"; \
         printf '#!/bin/sh\\nc=$(cat \"$d/pw\" 2>/dev/null); rm -rf \"$d\"; printf %s \"$c\"\\n' \
           > \"$d/askpass\"; chmod 700 \"$d/askpass\"; \
         {SFTP_SERVER_PROBE}\
         stty raw -echo -ixon -ixoff -onlcr 2>/dev/null; \
         SUDO_ASKPASS=\"$d/askpass\" sudo -k -A -u {user} \
           sh -c 'echo {READY_MARKER_TEXT}; exec \"$0\"' \"$srv\"; \
         rc=$?; rm -rf \"$d\"; exit \"$rc\""
    )
}

/// 口令行：`口令\n`，单独一次写通道，绝不与脚本拼进同一个字符串。
///
/// 只喂 `\n` 不喂 `\r`：此时 tty 仍是 canonical 模式，`\n` 是行结束符，而 `\r`
/// 会被 `read` 当成口令的最后一个字符，症状是「口令看着对却认证不过」。
fn password_line(password: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(format!("{password}\n").into_bytes())
}

/// 本地就能判定的输入检查。`pub(crate)` 是因为传输层要在**开通道之前**就调它：
/// 这些情况在远端的症状是「卡到超时」或「口令明明对却失败」，
/// 让一次通道往返替我们报错既慢又难懂。
pub(crate) fn validate_inputs(sudo_user: &str, password: &str) -> Result<(), SshError> {
    if sudo_user.trim().is_empty() {
        return Err(SshError::InvalidInput("提权用户不能为空".into()));
    }
    if password.is_empty() {
        return Err(SshError::InvalidInput("提权需要输入当前用户的 sudo 口令".into()));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(SshError::InvalidInput(format!(
            "sudo 口令最长 {MAX_PASSWORD_BYTES} 字节，请确认没有把整段文本粘贴进来"
        )));
    }
    Ok(())
}

/// 标记在已收到的字节里的状态。
#[derive(Debug, PartialEq, Eq)]
enum Marker {
    /// 还没出现过标记：继续等，同时这些字节都是「远端在喊话」的证据。
    Absent,
    /// 标记刚到行尾，换行还在下一个报文里。
    Pending,
    /// 标记 + 换行都到齐，且后面一个字节都没有：从这里起就是干净的 SFTP 线。
    Ready,
    /// 标记后面跟了别的东西。协议上不该出现，出现了也不能假装没看见。
    Dirty,
}

/// 在字节缓冲里找标记并判定状态。
///
/// 按字节而不是按字符串做：标记之前到达的是远端文本，但 pty 上什么编码都可能出现，
/// 用 `from_utf8_lossy` 之后的偏移量和原始字节已经对不上了。
fn marker_state(heard: &[u8]) -> Marker {
    let Some(at) = find_marker(heard) else { return Marker::Absent };
    let rest = &heard[at + READY_MARKER.len()..];
    let tail = match rest {
        // 标记刚到行尾；`[b'\r']` 是 `\r\n` 的第二个字节还在下一个报文里
        [] | [b'\r'] => return Marker::Pending,
        [b'\n', tail @ ..] | [b'\r', b'\n', tail @ ..] => tail,
        // 标记后面连换行都没等到就跟了别的东西，一样不干净
        _ => return Marker::Dirty,
    };
    if tail.is_empty() { Marker::Ready } else { Marker::Dirty }
}

/// 找**站在行首**的就绪标记。
///
/// 真机上它就是一句 `echo`，必然在行首；把这条写进匹配条件顺手挡掉一种冒充——
/// 用户口令恰好长成标记、而 tty 回显又没关掉时，那句回显总是跟在提示语后面，
/// 不在行首，于是不会被当成「提权已就绪」。
///
/// 每次都整段重扫（而不是记住上次扫到哪）：标记可能被 TCP 分段劈开，
/// 重扫才能让「前半截先到、后半截后到」这种形状最终认得出来。
fn find_marker(heard: &[u8]) -> Option<usize> {
    if heard.len() < READY_MARKER.len() {
        return None;
    }
    heard
        .windows(READY_MARKER.len())
        .enumerate()
        .find(|(i, w)| **w == READY_MARKER[..] && (*i == 0 || heard[*i - 1] == b'\n'))
        .map(|(i, _)| i)
}

/// 把远端在提权窗口里说的话翻成可行动的中文。
///
/// 口径与全项目一致：OS/工具的英文原文不直接外泄给用户，但也不能只回一句「失败了」。
/// 认不出的形状保留**抹掉口令之后**的最后一行原文——宁可让用户看见一句陌生的远端话，
/// 也不要应用替他编一个文不对题的原因。
pub fn classify_elevate_failure(text: &str, password: &str) -> String {
    let clean = redacted(text, password);
    let lower = clean.to_lowercase();
    if lower.contains("not in the sudoers file") || lower.contains("is not allowed to run") {
        return "远端 sudo 不允许当前用户使用 sudo，提权失败".to_string();
    }
    if lower.contains("a terminal is required") || lower.contains("no tty") {
        return "远端 sudo 要求必须有 tty 才能输入口令，提权失败".to_string();
    }
    if lower.contains("is not a setuid program") || lower.contains("must be owned by uid") {
        return "远端 sudo 的属主或权限不正确，提权失败".to_string();
    }
    if lower.contains("command not found") || lower.contains("dbhx-elevate: cannot create temp dir") {
        return "远端提权环境不完整（缺 sudo 或 mktemp），提权失败".to_string();
    }
    // sudo 的原文是「N incorrect password attempt(s)」，单复数都要认得住
    if lower.contains("incorrect password attempt") || lower.contains("try again") {
        return "sudo 口令不正确，提权失败".to_string();
    }
    if lower.contains("sudo:") {
        let tail = tail_line(&clean);
        return if tail.is_empty() {
            "远端 sudo 拒绝了这次提权".to_string()
        } else {
            format!("远端 sudo 报错：{tail}")
        };
    }
    if lower.contains("sftp-server") {
        return "远端没找到 sftp-server，提权失败".to_string();
    }
    "远端在提权过程中返回了无法识别的输出，提权失败".to_string()
}

/// 口令消除。空口令不替换——否则等于在每个字符位置插入标记。
fn redacted<'a>(text: &'a str, password: &str) -> Cow<'a, str> {
    if password.is_empty() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace(password, "<口令已隐去>"))
}

/// 取最后一行非空文本并限长。远端报错的有效信息通常就在最后一行。
fn tail_line(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(MAX_CHARS).collect()
}

/// 在一条 pty + exec 的通道上跑完提权握手，成功就把裸流交出去。
///
/// 单独成函数而不是塞进 actor 的 `handle_request` 分支：这一段是「发请求、写字节、
/// 读一段、再决定给不给流」的编排，混进 match 里会把唯一一处「等就绪标记期间
/// 远端可能在喊话」的特殊时序藏起来。
///
/// 通道按值接收（[`crate::channel::WireChannel`] 的其它用法都是 `&mut`）：成功那一刻
/// 要把整个通道 `into_stream()` 交出去，借用的通道动不了。
pub(crate) async fn elevated_open(
    mut channel: Channel<Msg>,
    endpoint: &LiveEndpoint,
    sudo_user: &str,
    password: &str,
) -> Result<ChannelStream<Msg>, SshError> {
    validate_inputs(sudo_user, password)?;

    channel
        .request_pty(true, ELEVATE_TERM, ELEVATE_COLS, ELEVATE_ROWS, 0, 0, &[])
        .await?;
    // pty 这一步仍然只认 `Success`：此时远端还没起任何进程，没有「命令跑起来了但
    // 没干成事」这种中间态。`request_*` 只要消息发得出去就返回 Ok，服务器稍后拒绝
    // 不会让这个返回值变红（见 channel.rs 模块注释），所以确认必须自己等。
    await_channel_success(&mut channel, endpoint, password).await?;
    channel
        .exec(true, build_exec_command(sudo_user).as_bytes())
        .await?;
    // 口令必须在 exec 之后、且在远端 `read` 之前到达。这两句的顺序不能反，也不能
    // 并发：`read` 会一直阻塞到这一行出现，所以先发 exec 再写口令是安全的；
    // 反过来写则会把口令行喂给还没启动的 shell 之外的读者。
    let line = password_line(password);
    channel.data_bytes(line.to_vec()).await?;
    drop(line);

    match await_ready(&mut channel, endpoint, password).await {
        Ok(()) => Ok(channel.into_stream()),
        Err(e) => {
            // 失败必须关掉通道：留一条远端 bash 挂在 `read` 上的僵尸通道，比多拨一次
            // 连接更糟——它占着 sshd 的一个 session 不放。
            let _ = channel.close().await;
            Err(e)
        }
    }
}

/// 等到通道请求的 `Success`（pty 阶段用）。
async fn await_channel_success(
    channel: &mut Channel<Msg>,
    endpoint: &LiveEndpoint,
    password: &str,
) -> Result<(), SshError> {
    loop {
        match recv(channel, endpoint, password, &[]).await? {
            ChannelMsg::Success => return Ok(()),
            ChannelMsg::Failure => {
                return Err(SshError::ElevateDenied("远端 sshd 拒绝了提权所需的终端请求".into()));
            }
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                tracing::debug!(count = data.len(), "pty 确认前收到意外数据，已丢弃");
            }
            other => {
                tracing::debug!(?other, "pty 确认前收到不影响判定的消息，继续等待 Success");
            }
        }
    }
}

/// 等到「提权后的 shell 把就绪标记写完」。
///
/// `Success` 在这里只是继续等（见模块注释：它只证明命令被起起来了）。真正定性失败的
/// 是 `Failure`、进程退出、通道关闭、超时，以及标记之前那一段喊话。
async fn await_ready(
    channel: &mut Channel<Msg>,
    endpoint: &LiveEndpoint,
    password: &str,
) -> Result<(), SshError> {
    let mut heard: Vec<u8> = Vec::new();
    loop {
        let msg = recv(channel, endpoint, password, &heard).await?;
        match msg {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                heard.extend_from_slice(&data);
                match marker_state(&heard) {
                    Marker::Ready => return Ok(()),
                    Marker::Absent | Marker::Pending => {}
                    Marker::Dirty => {
                        tracing::debug!(
                            host = %endpoint.host,
                            text = %redacted(&String::from_utf8_lossy(&heard), password),
                            "就绪标记后面跟着别的字节"
                        );
                        return Err(SshError::ElevateDenied(
                            "远端在提权输出里混入了非协议字节，提权中止".into(),
                        ));
                    }
                }
            }
            // 命令被接受了，但 sudo 还没说话：继续等标记
            ChannelMsg::Success => {}
            ChannelMsg::Failure => {
                return Err(denied(&heard, password, "远端 sshd 拒绝了这个提权请求"));
            }
            ChannelMsg::ExitStatus { exit_status } => {
                tracing::debug!(
                    host = %endpoint.host,
                    exit_status,
                    text = %redacted(&String::from_utf8_lossy(&heard), password),
                    "提权进程退出"
                );
                return Err(denied(
                    &heard,
                    password,
                    &format!("远端提权进程以退出码 {exit_status} 结束，未给出原因"),
                ));
            }
            // Eof/Close 由 `recv` 收成 `SshError::Closed` 送回来，在这里归类成提权失败
            other => {
                tracing::debug!(?other, "提权通道收到不影响判定的消息，继续等待就绪标记");
            }
        }
    }
}

/// 取一条通道消息，超时与通道尽头各自归位。
///
/// 超时翻成 `ElevateDenied` 而不是 `SshError::Timeout`：后者过 IPC 是「连接超时」，
/// 而这条连接明明活着、只是提权没被确认。用户看见「超时」会去查网络，实际该查的是远端 sudo。
///
/// `heard` 是已经收到的喊话，只用于日志与失败定性；pty 阶段传空切片。
async fn recv(channel: &mut Channel<Msg>, endpoint: &LiveEndpoint, password: &str, heard: &[u8]) -> Result<ChannelMsg, SshError> {
    match timeout(ELEVATE_TIMEOUT, channel.wait()).await {
        Err(_elapsed) => {
            tracing::debug!(
                host = %endpoint.host,
                text = %redacted(&String::from_utf8_lossy(heard), password),
                "提权等待超时，远端没有确认请求"
            );
            Err(SshError::ElevateDenied(
                "远端在提权请求上没有回应，提权失败".to_string(),
            ))
        }
        Ok(Some(msg)) => Ok(msg),
        // 通道尽头：`wait()` 把 Eof/Close 都收成 None
        Ok(None) => Err(denied(heard, password, "远端在提权过程中关闭了通道，未给出原因")),
    }
}

/// 有远端原话就按原话定性，没有才回落到通用原因。
fn denied(heard: &[u8], password: &str, fallback: &str) -> SshError {
    if heard.iter().all(u8::is_ascii_whitespace) {
        return SshError::ElevateDenied(fallback.to_string());
    }
    SshError::ElevateDenied(classify_elevate_failure(&String::from_utf8_lossy(heard), password))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_text() -> &'static str {
        READY_MARKER_TEXT
    }

    #[test]
    fn quoting_wraps_the_argument_and_survives_embedded_quotes() {
        assert_eq!(shell_quote("root"), "'root'");
        // `'; rm -rf /; '` 这类输入必须被关在引号里，而不是变成第二条命令
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn the_password_never_appears_in_the_command_string() {
        // 口令唯一的样子是 `read` 从 stdin 取的那一行；命令串里出现它的任何一个
        // 变体都意味着它会进远端的进程表。
        let cmd = build_exec_command("root");
        assert!(cmd.contains("IFS= read -r pw"), "{cmd}");
        for fragment in ["hunter2", "correct horse", "sudo -S"] {
            assert!(!cmd.contains(fragment), "命令串里出现了 {fragment}");
        }
    }

    #[test]
    fn the_command_line_carries_no_newline() {
        // 换行会把「读口令那一行」和脚本正文混在同一条 stdin 上
        let cmd = build_exec_command("root");
        assert!(!cmd.contains('\n'), "{cmd}");
    }

    #[test]
    fn a_quote_in_the_sudo_user_cannot_break_out_of_the_argument() {
        let cmd = build_exec_command("root'; rm -rf /; 'x");
        // 引号被逐段关起来之后，整条命令仍然只有一个 `sudo -u` 参数位
        assert!(cmd.contains("-u 'root'\\''; rm -rf /; '\\''x'"), "{cmd}");
        assert_eq!(cmd.matches("sudo -k -A -u").count(), 1, "{cmd}");
    }

    #[test]
    fn the_ready_marker_is_printed_from_inside_the_elevated_process() {
        // 这是整条设计的关键顺序：标记在 sudo 之后、由被提权的那个 shell 打印，
        // 所以「看见标记」等价于「认证过且 sudoers 放行」。
        let cmd = build_exec_command("root");
        let echo_at = cmd
            .find(&format!("echo {}", marker_text()))
            .expect("标记要由远端 echo 出来");
        let sudo_at = cmd.find("sudo -k -A").expect("必须有 sudo");
        assert!(echo_at > sudo_at, "echo 必须在 sudo 之后：{cmd}");
        assert!(cmd.contains("exec \"$0\""), "打印完就换成 sftp-server，不能多留一个 shell：{cmd}");
        assert!(cmd.contains("\"$srv\""), "{cmd}");
    }

    #[test]
    fn the_elevated_server_is_started_without_stderr_logging() {
        // `-e` 会把 sftp-server 的日志写到 stderr，而 pty 通道里 stderr 与 stdout
        // 是同一条字节流——日志一进来，SFTP 报文就毁了。
        let cmd = build_exec_command("root");
        assert!(
            !cmd.split(|c: char| c.is_whitespace() || c == ';').any(|tok| tok == "-e"),
            "提权起的 sftp-server 不能带 -e：{cmd}"
        );
    }

    #[test]
    fn the_tty_is_set_raw_before_the_server_starts() {
        let cmd = build_exec_command("root");
        let raw_at = cmd.find("stty raw").expect("必须切 raw");
        let sudo_at = cmd.find("sudo -k -A").expect("必须有 sudo");
        assert!(raw_at < sudo_at, "raw 必须早于 sudo，否则报文会被 tty 改写");
        for flag in ["-ixon", "-ixoff", "-onlcr"] {
            assert!(cmd.contains(flag), "缺少 {flag}：{cmd}");
        }
    }

    #[test]
    fn echo_is_disabled_before_the_password_is_read() {
        let cmd = build_exec_command("root");
        let echo_off_at = cmd.find("stty -echo").expect("必须先关回显");
        let write_pw_at = cmd.find("> \"$d/pw\"").expect("口令必须落到临时文件");
        assert!(echo_off_at < write_pw_at, "回显要在口令落地之前关掉");
    }

    #[test]
    fn askpass_and_exit_paths_both_clean_the_temp_dir() {
        let cmd = build_exec_command("root");
        // askpass 读罢即焚；免密机器上 askpass 不运行，所以退出前还要再清一次
        assert!(cmd.contains("rm -rf \"$d\"; printf %s"), "{cmd}");
        assert!(cmd.contains("rc=$?; rm -rf \"$d\"; exit \"$rc\""), "{cmd}");
    }

    #[test]
    fn password_line_ends_with_lf_only() {
        let line = password_line("hunter2");
        assert_eq!(&line[..], b"hunter2\n");
    }

    #[test]
    fn overlong_or_empty_inputs_are_refused_locally() {
        // 空口令在远端的症状是「卡住直到超时」，超长是「明明对却失败」，都不如当场拒
        let too_long = "x".repeat(MAX_PASSWORD_BYTES + 1);
        let cases: [(&str, &str); 4] =
            [("", "hunter2"), ("  ", "hunter2"), ("root", ""), ("root", &too_long)];
        for (user, password) in cases {
            let err = validate_inputs(user, password).expect_err("这一组必须在本地被拒");
            assert!(matches!(err, SshError::InvalidInput(_)), "user={user:?} 长度={}：{err:?}", password.len());
        }
        assert!(validate_inputs("root", &"x".repeat(MAX_PASSWORD_BYTES)).is_ok());
    }

    #[test]
    fn marker_state_needs_the_line_break_before_it_declares_ready() {
        let m = marker_text();
        assert_eq!(marker_state(b"sudo: nope\n"), Marker::Absent);
        assert_eq!(marker_state(m.as_bytes()), Marker::Pending);
        assert_eq!(marker_state(format!("{m}\r").as_bytes()), Marker::Pending);
        assert_eq!(marker_state(format!("{m}\n").as_bytes()), Marker::Ready);
        assert_eq!(marker_state(format!("{m}\r\n").as_bytes()), Marker::Ready);
        // 提示符之类抢在后面：这条线已经不纯了，不能交出去
        assert_eq!(marker_state(format!("{m}\n$ ").as_bytes()), Marker::Dirty);
        assert_eq!(marker_state(format!("{m}x").as_bytes()), Marker::Dirty);
    }

    #[test]
    fn the_marker_survives_being_split_across_packets() {
        // 通道按 TCP 分段投递，标记完全可能被劈成两半；只要拼上就该认得
        let full = format!("{}\n", marker_text());
        let bytes = full.as_bytes();
        assert_eq!(marker_state(&bytes[..10]), Marker::Absent);
        assert_eq!(marker_state(&bytes[..bytes.len() - 1]), Marker::Pending);
        assert_eq!(marker_state(bytes), Marker::Ready);
    }

    #[test]
    fn only_a_line_anchored_marker_counts() {
        let m = marker_text();
        assert_eq!(find_marker(m.as_bytes()), Some(0));
        assert_eq!(find_marker(format!("sudo: {m}\n").as_bytes()), None);
        assert_eq!(find_marker(format!("prompt: {m}\n").as_bytes()), None);
        assert_eq!(find_marker(format!("noise\n{m}\n").as_bytes()), Some(6));
        assert_eq!(find_marker(b"short"), None);
    }

    fn classify(text: &str) -> String {
        classify_elevate_failure(text, "hunter2")
    }

    #[test]
    fn sudoers_rejection_is_named_as_sudoers_rejection() {
        let msg = classify("app is not in the sudoers file. This incident will be reported.");
        assert!(msg.contains("不允许"), "{msg}");
        assert!(msg.contains("sudo"), "{msg}");
    }

    #[test]
    fn requiretty_is_reported_as_a_remote_sudo_policy() {
        let msg = classify("sudo: a terminal is required to read the password");
        assert!(msg.contains("tty"), "{msg}");
    }

    #[test]
    fn a_wrong_password_is_told_apart_from_a_policy_block() {
        assert!(classify("sudo: 1 incorrect password attempt").contains("口令不正确"));
        assert!(classify("sudo: 3 incorrect password attempts").contains("口令不正确"));
        assert!(classify("Sorry, try again.").contains("口令不正确"));
    }

    #[test]
    fn missing_sudo_is_not_blamed_on_the_user() {
        let msg = classify("bash: sudo: command not found");
        assert!(msg.contains("没有安装 sudo") || msg.contains("环境不完整"), "{msg}");
    }

    #[test]
    fn the_password_never_survives_into_the_message() {
        // 真机上 tty 回显过口令时，这段话会整段进报错原文：一条都不能漏出去
        let leaked = "Password: hunter2\nsudo: hunter2 is not in the sudoers file";
        for msg in [classify(leaked), classify_elevate_failure(leaked, "")] {
            assert!(!msg.contains("hunter2"), "报错里出现了口令：{msg}");
        }
    }

    #[test]
    fn an_unrecognised_sudo_line_keeps_the_remote_text_but_redacts_it() {
        let msg = classify("sudo: pam_tally2: something we have never seen about hunter2");
        assert!(msg.contains("远端 sudo 报错"), "{msg}");
        assert!(msg.contains("pam_tally2"), "原文要留住，别替用户编原因：{msg}");
        assert!(!msg.contains("hunter2"), "{msg}");
    }

    #[test]
    fn total_silence_still_gets_a_chinese_reason() {
        let msg = classify("");
        assert!(!msg.is_empty());
        assert!(msg.chars().any(|c| c >= '一' && c <= '龥'), "{msg}");
    }

    #[test]
    fn silence_or_whitespace_alone_falls_back_to_the_generic_reason() {
        // 「远端只发了个换行」是 NOPASSWD 机器上的常态，不该被拿去猜原因
        for bytes in [&b""[..], b"\r\n  \n"] {
            let err = denied(bytes, "hunter2", "远端 sshd 拒绝了这个提权请求");
            assert_eq!(err.to_string(), "远端 sshd 拒绝了这个提权请求", "{err:?}");
        }
        let err = denied(b"sudo: app is not in the sudoers file", "hunter2", "通用");
        assert!(err.to_string().contains("不允许"), "{err}");
    }

    #[test]
    fn tail_line_truncates_and_picks_the_last_nonempty_line() {
        assert_eq!(tail_line("a\n\n  boom  \n"), "boom");
        let long = "x".repeat(500);
        assert_eq!(tail_line(&long).len(), 200);
    }

    #[test]
    fn redaction_skips_the_empty_password() {
        // 空口令去 replace 会在每个位置插入标记（Rust 的 replace 对空 pattern 就是这个行为）
        assert_eq!(redacted("plain", ""), "plain");
        assert_eq!(redacted("a hunter2 b", "hunter2"), "a <口令已隐去> b");
    }
}
