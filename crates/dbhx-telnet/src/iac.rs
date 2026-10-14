//! telnet IAC 协议：命令解析（含跨包重组与 0xFF 转义）与协商策略。
//!
//! telnet 的难点不在「连上」，而在 `0xFF`（IAC）既是数据里的一个字节又是对话的引导符，
//! 而且一条对话可以被 TCP 分段切在任何位置。`IacParser` 是一个可跨 chunk 存活的
//! 状态机：它把「哪些字节进终端、哪些是对话命令」在流上一刀切干净——IAC 帧绝不能
//! 漏进 xterm，否则一条 `IAC WILL` 会在屏幕上留下乱码。协商策略与传输分离成纯函数，
//! 因此可以在没有任何 socket 的单测里逐条钉死。

/// 命令与转义引导符。
pub const IAC: u8 = 255;
/// 选项协商：我方的。
pub const WILL: u8 = 251;
pub const WONT: u8 = 252;
/// 选项协商：请求对端的。
pub const DO: u8 = 253;
pub const DONT: u8 = 254;
/// 子协商开始 / 结束。
pub const SB: u8 = 250;
pub const SE: u8 = 240;

/// 常见选项号。策略只关心这几个，其余一律礼貌拒绝。
pub const OPT_ECHO: u8 = 1;
pub const OPT_SGA: u8 = 3;
pub const OPT_STATUS: u8 = 5;
pub const OPT_NAWS: u8 = 31;

/// 一条从流里解析出来的对端命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetCommand {
    /// 对端声明它将启用某选项。
    Will(u8),
    /// 对端声明它将停用某选项。
    Wont(u8),
    /// 对端请求我方启用某选项。
    Do(u8),
    /// 对端请求我方停用某选项。
    Dont(u8),
    /// 子协商（`IAC SB <opt> …data… IAC SE`）。
    Subnegotiation { option: u8, data: Vec<u8> },
    /// 其余单字节命令（GA、EL、AYT、AO 之类），此处只透传编号、不响应。
    Other(u8),
}

/// `feed` 一次的结果：可以安全进终端的原始字节，和解析出的命令序列。
#[derive(Debug, Default, Clone)]
pub struct Parsed {
    pub data: Vec<u8>,
    pub commands: Vec<TelnetCommand>,
}

#[derive(Debug)]
enum State {
    /// 正常数据流。
    Data,
    /// 读到 `IAC`，下一个字节决定它是转义还是命令。
    AfterIac,
    /// 读到 `IAC <WILL|WONT|DO|DONT>`，下一个字节是选项号。
    AwaitOption(u8),
    /// 读到 `IAC SB`，下一个字节是子协商的选项号。
    SbOption,
    /// 在子协商里收集数据，直到 `IAC SE`。
    SbData { option: u8, data: Vec<u8>, saw_iac: bool },
}

impl Default for State {
    fn default() -> Self {
        State::Data
    }
}

/// 跨 chunk 存活的 telnet 解析器。
#[derive(Debug, Default)]
pub struct IacParser {
    state: State,
}

impl IacParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂进一段到达字节，返回其中「属于终端数据」的部分与解析出的命令。
    /// 半条命令会留在状态里，等下一次 `feed` 续上。
    pub fn feed(&mut self, chunk: &[u8]) -> Parsed {
        let mut out = Parsed::default();
        for &b in chunk {
            self.state = match std::mem::replace(&mut self.state, State::Data) {
                State::Data => {
                    if b == IAC {
                        State::AfterIac
                    } else {
                        out.data.push(b);
                        State::Data
                    }
                }
                State::AfterIac => match b {
                    // `IAC IAC` 是数据里真正的 0xFF，不是命令
                    IAC => {
                        out.data.push(IAC);
                        State::Data
                    }
                    WILL | WONT | DO | DONT => State::AwaitOption(b),
                    SB => State::SbOption,
                    // 其余单字节命令：GA / 越界的 SE / AYT / … 当作 Other 透传编号
                    other => {
                        out.commands.push(TelnetCommand::Other(other));
                        State::Data
                    }
                },
                State::AwaitOption(cmd) => {
                    out.commands.push(command_from(cmd, b));
                    State::Data
                }
                State::SbOption => State::SbData { option: b, data: Vec::new(), saw_iac: false },
                State::SbData { option, mut data, saw_iac } => {
                    if saw_iac {
                        match b {
                            SE => {
                                out.commands.push(TelnetCommand::Subnegotiation { option, data });
                                State::Data
                            }
                            // 子协商里 `IAC IAC` 是转义出来的一个 0xFF
                            IAC => {
                                data.push(IAC);
                                State::SbData { option, data, saw_iac: false }
                            }
                            // 罕见的非法序列：宽松把 IAC 与当前字节都当数据收下
                            other => {
                                data.push(IAC);
                                data.push(other);
                                State::SbData { option, data, saw_iac: false }
                            }
                        }
                    } else if b == IAC {
                        State::SbData { option, data, saw_iac: true }
                    } else {
                        data.push(b);
                        State::SbData { option, data, saw_iac: false }
                    }
                }
            };
        }
        out
    }
}

fn command_from(cmd: u8, option: u8) -> TelnetCommand {
    match cmd {
        WILL => TelnetCommand::Will(option),
        WONT => TelnetCommand::Wont(option),
        DO => TelnetCommand::Do(option),
        DONT => TelnetCommand::Dont(option),
        _ => TelnetCommand::Other(option),
    }
}

/// 把用户输入写回 telnet 之前必须转义裸 `0xFF`：否则一个 0xFF 会被对端当成 IAC，
/// 把后面的字节读成命令。这是数据方向的转义，与解析方向（`IacParser`）对称。
pub fn escape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b == IAC {
            out.push(IAC);
        }
        out.push(b);
    }
    out
}

/// 组一条 `IAC <cmd> <opt>` 的双字节命令。
pub fn iac_option(cmd: u8, option: u8) -> Vec<u8> {
    vec![IAC, cmd, option]
}

/// 组一条 NAWS 子协商（窗口宽高各一个大端 u16）。
pub fn naws(cols: u16, rows: u16) -> Vec<u8> {
    let (cw, ch) = (cols.to_be_bytes(), rows.to_be_bytes());
    vec![IAC, SB, OPT_NAWS, ch[0], ch[1], cw[0], cw[1], IAC, SE]
}

/// 协商策略：对端一条命令，回什么（可能是空，即「不该插话」）。
///
/// 立场是一个克制的原始终端客户端：只声明自己会报窗口大小（NAWS），
/// 接受对端回声与抑制转义（ECHO/SGA）好让 line discipline 留在服务端，
/// 其余选项一律婉拒——多答应一项就多一处「我们以为能协商其实不会处理」的地雷。
pub fn reply_to(cmd: &TelnetCommand, cols: u16, rows: u16) -> Vec<u8> {
    match cmd {
        // 对端要启用某选项：ECHO/SGA/STATUS 接受（DO），其余请它别做（DONT）
        TelnetCommand::Will(opt) if matches!(*opt, OPT_ECHO | OPT_SGA | OPT_STATUS) => iac_option(DO, *opt),
        TelnetCommand::Will(opt) => iac_option(DONT, *opt),
        // 对端停用选项：无需回应
        TelnetCommand::Wont(_) => Vec::new(),
        // 对端要我启用某选项：只有 NAWS 我们会做，做完顺手把当前窗口报过去
        TelnetCommand::Do(OPT_NAWS) => {
            let mut out = iac_option(WILL, OPT_NAWS);
            out.extend_from_slice(&naws(cols, rows));
            out
        }
        TelnetCommand::Do(opt) => iac_option(DONT, *opt),
        TelnetCommand::Dont(_) => Vec::new(),
        // 子协商与单字节命令：透传期间不回应答（STATUS 查询的应答属于更完整的实现）
        TelnetCommand::Subnegotiation { .. } | TelnetCommand::Other(_) => Vec::new(),
    }
}

/// 连上之后我方主动发的问候：声明 WILL NAWS 并立刻报一次窗口大小。
pub fn greeting(cols: u16, rows: u16) -> Vec<u8> {
    let mut out = iac_option(WILL, OPT_NAWS);
    out.extend_from_slice(&naws(cols, rows));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_data_passes_through_untouched() {
        let mut p = IacParser::new();
        let r = p.feed(b"ls -la\r\n");
        assert_eq!(r.data, b"ls -la\r\n");
        assert!(r.commands.is_empty());
    }

    #[test]
    fn a_bare_ff_in_data_survives_the_escape() {
        let mut p = IacParser::new();
        let r = p.feed(&[b'A', IAC, IAC, b'B']);
        assert_eq!(r.data, [b'A', IAC, b'B'], "IAC IAC 必须还原成一个 0xFF 数据字节");
        assert!(r.commands.is_empty());
    }

    #[test]
    fn negotiation_never_leaks_into_terminal_data() {
        let mut p = IacParser::new();
        let r = p.feed(&[IAC, WILL, OPT_ECHO, b'x', IAC, DO, OPT_NAWS]);
        assert_eq!(r.data, b"x", "只有非命令字节进终端");
        assert_eq!(r.commands, [TelnetCommand::Will(OPT_ECHO), TelnetCommand::Do(OPT_NAWS)]);
    }

    #[test]
    fn a_command_split_across_chunks_is_reassembled() {
        // TCP 把 `IAC WILL ECHO` 切成两段是常态，不是错误
        let mut p = IacParser::new();
        let first = p.feed(&[IAC, WILL]);
        assert!(first.commands.is_empty(), "选项号还没到，不该下结论");
        let second = p.feed(&[OPT_ECHO]);
        assert_eq!(second.commands, [TelnetCommand::Will(OPT_ECHO)]);
    }

    #[test]
    fn subnegotiation_collects_until_iac_se_and_ignores_escaped_ff() {
        let mut p = IacParser::new();
        let mut stream = vec![IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, IAC, 7, IAC, SE];
        let r = p.feed(&stream);
        assert!(r.data.is_empty(), "子协商内容不进终端");
        assert_eq!(r.commands, [TelnetCommand::Subnegotiation { option: OPT_NAWS, data: vec![0, 80, 0, 24, IAC, 7] }]);
        stream.clear();
        // 解析完应干净回到数据态：后面就是普通数据
        assert_eq!(p.feed(b"hi").data, b"hi");
    }

    #[test]
    fn outgoing_data_escapes_every_ff() {
        assert_eq!(escape(&[b'a', IAC, b'b', IAC, IAC]), [b'a', IAC, IAC, b'b', IAC, IAC, IAC, IAC]);
    }

    #[test]
    fn will_echo_is_accepted_but_unknown_options_are_declined() {
        assert_eq!(reply_to(&TelnetCommand::Will(OPT_ECHO), 80, 24), iac_option(DO, OPT_ECHO));
        assert_eq!(reply_to(&TelnetCommand::Will(99), 80, 24), iac_option(DONT, 99));
    }

    #[test]
    fn being_asked_to_do_naws_answers_with_will_and_current_size() {
        let out = reply_to(&TelnetCommand::Do(OPT_NAWS), 80, 24);
        assert_eq!(&out[..3], &[IAC, WILL, OPT_NAWS]);
        assert_eq!(&out[3..], &naws(80, 24)[..]);
    }

    #[test]
    fn being_asked_to_do_anything_else_is_a_polite_no() {
        assert_eq!(reply_to(&TelnetCommand::Do(OPT_ECHO), 80, 24), iac_option(DONT, OPT_ECHO));
    }

    #[test]
    fn wont_and_dont_from_peer_require_no_reply() {
        assert!(reply_to(&TelnetCommand::Wont(OPT_ECHO), 80, 24).is_empty());
        assert!(reply_to(&TelnetCommand::Dont(OPT_NAWS), 80, 24).is_empty());
    }
}
