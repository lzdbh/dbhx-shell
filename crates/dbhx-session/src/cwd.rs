//! OSC 7 跟随：从终端输出字节流里提出「 shell 现在的目录」。
//!
//! 这是 SFTP 面板免登录跳转的关键：用户在终端里 `cd /var/log`，文件面板就该跟着过去，
//! 而这中间没有任何协议字段可查——唯一可靠来源是 shell 自己在提示符里打的 OSC 7。
//!
//! 三个必须自己扛的坑，都是这里在做的事：
//!
//! * **序列会被读缓冲劈开**。一次 `read` 到 `"\x1b[6"` 这种半截是常态，
//!   所以解析器是跨调用的状态机，不是 `String::from_utf8` 之后再找。
//! * **误报比漏报有害**。漏一次只是面板没跳，误报会把用户手动浏览的目录拽走。
//!   因此只认 `OSC 7`，长度超限或结构不对就整条丢掉，并且同一目录不重复上报。
//! * **字节流里混着别的转义序列**。`OSC 0;标题` 长得和它只差一个数字，
//!   所以先按字节进状态机，再单独校验负载。

/// 单条 OSC 负载上限。路径 URI 实际几百字节；到这个长度说明状态机被垃圾数据带偏了，
/// 主动丢弃重新同步，不能让缓冲无限长下去。
const MAX_PAYLOAD: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// 正常文本，只在等 `ESC`
    #[default]
    Ground,
    /// 刚收到 `ESC`，下一个字节决定它是 OSC 还是别的转义
    AfterEsc,
    /// 在 OSC 负载里攒字节
    InOsc,
    /// OSC 负载里收到 `ESC`，等 `\` 组成 ST 终止符
    OscEsc,
}

/// OSC 7 跟踪器。喂终端输出的原始字节，得到目录变化。
#[derive(Debug, Default)]
pub struct CwdTracker {
    state: State,
    payload: Vec<u8>,
    /// 上次对外报告过的目录，用于去重
    last: Option<String>,
}

impl CwdTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前已知的目录。`None` = 服务器还没告过，别猜。
    pub fn current(&self) -> Option<&str> {
        self.last.as_deref()
    }

    /// 喂一段字节。返回本次**新**解析出的目录；与上次相同则返回 `None`。
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut changed = None;
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.state = State::AfterEsc;
                    }
                }
                State::AfterEsc => match b {
                    OSC_INTRO => {
                        self.payload.clear();
                        self.state = State::InOsc;
                    }
                    // 连续两个 ESC：前一个是噪声，后一个才是真的起始
                    ESC => self.state = State::AfterEsc,
                    // CSI/OSC 之外的转义我们不需要理解，交回地面状态继续等 ESC
                    _ => self.state = State::Ground,
                },
                State::InOsc => match b {
                    BEL => changed = self.finish(),
                    ESC => self.state = State::OscEsc,
                    _ => {
                        if self.payload.len() >= MAX_PAYLOAD {
                            tracing::debug!("OSC 负载超长，丢弃并重新同步");
                            self.reset();
                        } else {
                            self.payload.push(b);
                        }
                    }
                },
                State::OscEsc => {
                    if b == ST_INTRO {
                        changed = self.finish();
                    } else {
                        // OSC 负载里不该出现 ESC；后面跟的不是 `\` 说明整条不可信
                        self.reset();
                    }
                }
            }
        }
        changed
    }

    /// 丢弃在攒的负载，回到地面状态。终止符之外的任何异常都走这里。
    fn reset(&mut self) {
        self.payload.clear();
        self.state = State::Ground;
    }

    fn finish(&mut self) -> Option<String> {
        let parsed = parse_cwd(&self.payload);
        self.reset();
        let path = parsed?;
        if self.last.as_deref() == Some(path.as_str()) {
            return None;
        }
        self.last = Some(path.clone());
        Some(path)
    }
}

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const OSC_INTRO: u8 = b']';
/// `ESC \` 的第二字节。Xterm 规范里 OSC 有两种终止：BEL 与 ST，服务器实现两种都常见。
const ST_INTRO: u8 = b'\\';

/// 解析 OSC 负载。只认 `7;...`，其余一律 `None`。
///
/// 接受三种写法：
/// * `file://host/a/b` —— 标准形态，host 段丢弃（它可能因为 `/etc/hostname` 而不可信，
///   而且我们要的只是路径）；
/// * `file:///a/b` —— 省略 host；
/// * `/a/b` —— 少数提示符实现直接打路径。
///
/// 百分号编码要还原，否则 `/home/my%20user` 在 SFTP 侧是个不存在的路径。
///
/// 结果必须是绝对路径：相对路径、空串、没有前导 `/` 的怪东西一律丢弃。
/// 误报一次目录就会把用户在文件面板里手动浏览的位置拽走，代价远大于漏报。
fn parse_cwd(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    let (number, rest) = text.split_once(';')?;
    if number != "7" || rest.is_empty() {
        return None;
    }
    let raw = match rest.strip_prefix("file://") {
        // 去掉 `//` 到第一个 `/` 之间的 host 段
        Some(after) => match after.find('/') {
            // authority 段里出现 `:` 说明那不是主机名而是 Windows 盘符
            // （`file://C:/Users`）。此时整条 URI 已经无法可靠拆分，直接丢弃——
            // 猜成 `/Users` 会把用户送到一个根本不存在的目录。
            Some(idx) if !after[..idx].contains(':') => &after[idx..],
            _ => return None,
        },
        None => rest,
    };
    let mut path = percent_decode(raw);
    // 去掉尾部斜杠，但根目录 `/` 不能被削成空串
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    if !path.starts_with('/') {
        return None;
    }
    Some(path)
}

/// 只处理 `%XX`，其它字节原样拷贝。不做 UTF-8 之外的字符集猜测。
///
/// 非法转义（`%zz`、结尾孤零零的 `%`）**按字面保留**而不是报错丢弃：
/// 真实路径里出现文件名含 `%` 的情况并不少见，宁可给回原样让用户看得见。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &bytes[i + 1..i + 3];
            if let Some(v) = hex::decode_pair(hex) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // 输入来自 from_utf8 成功的切片，解码后的字节仍应是合法 UTF-8；
    // 万一不是（服务器给了 GB18030 的路径），lossy 兜底而不是 panic。
    String::from_utf8_lossy(&out).into_owned()
}

mod hex {
    /// 两个 ASCII 字节 ⇒ 一个字节。任一不是十六进制就返回 `None`。
    pub(super) fn decode_pair(pair: &[u8]) -> Option<u8> {
        let hi = nibble(pair[0])?;
        let lo = nibble(pair[1])?;
        Some(hi << 4 | lo)
    }

    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(payload: &str) -> String {
        format!("\x1b]{payload}\x07")
    }

    #[test]
    fn standard_uri_form_yields_the_path() {
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(osc("7;file://db1/var/log").as_bytes()).as_deref(), Some("/var/log"));
        assert_eq!(t.current(), Some("/var/log"));
    }

    #[test]
    fn host_component_is_dropped_and_missing_host_is_fine() {
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(osc("7;file://anywhere/home/app").as_bytes()).as_deref(), Some("/home/app"));
        let mut bare = CwdTracker::new();
        assert_eq!(bare.feed(osc("7;file:///srv/data").as_bytes()).as_deref(), Some("/srv/data"));
    }

    #[test]
    fn st_terminator_works_like_bel() {
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(b"\x1b]7;file://h/a\x1b\\").as_deref(), Some("/a"));
    }

    #[test]
    fn windows_drive_path_keeps_the_leading_slash() {
        // Windows 上的 openssh 打的是 `file://H/C:/Users`：去掉 host 段之后剩下的就是
        // `/C:/Users`，与 SFTP 侧 `realpath` 给回的形态一致，所以原样保留、不做「去斜杠」翻译。
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(osc("7;file://H/C:/Users/app").as_bytes()).as_deref(), Some("/C:/Users/app"));
        // 没有 host 段的写法会落在 `C:` 之前，无法与绝对路径区分，丢弃
        let mut weird = CwdTracker::new();
        assert_eq!(weird.feed(osc("7;file://C:/Users").as_bytes()), None);
    }

    #[test]
    fn bare_path_form_is_accepted() {
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(osc("7;/tmp/x").as_bytes()).as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn sequence_split_across_reads_is_still_parsed() {
        // 每个切分点都要能解析出同一个目录——这是本模块存在的主要理由
        let stream = format!("welcome {}\x1b[?2004h", osc("7;file://h/etc/ssh"));
        let bytes = stream.as_bytes();
        for cut in 1..bytes.len() {
            let mut t = CwdTracker::new();
            let first = t.feed(&bytes[..cut]);
            let second = t.feed(&bytes[cut..]);
            let reported = match (first, second) {
                (Some(_), Some(_)) => panic!("偏移 {cut} 处重复上报，每次切块都会触发面板跳转"),
                (Some(p), None) | (None, Some(p)) => Some(p),
                (None, None) => None,
            };
            assert_eq!(reported.as_deref(), Some("/etc/ssh"), "在偏移 {cut} 处劈开后丢了目录");
        }
    }

    #[test]
    fn other_osc_numbers_are_ignored() {
        // OSC 0 设窗口标题，形状几乎一样，误认会把标题当目录
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(osc("0;file://h/should/be/ignored").as_bytes()), None);
        assert_eq!(t.current(), None);
        let mut five = CwdTracker::new();
        assert_eq!(five.feed(osc("5;118;119").as_bytes()), None);
    }

    #[test]
    fn malformed_payloads_are_dropped_not_guessed() {
        let cases = [
            "7;file://hostonly", // 没有路径段
            "7;",                // 空
            "7;a:b:c",           // 不是绝对路径
            "7",                 // 连分隔符都没有
        ];
        for case in cases {
            let mut t = CwdTracker::new();
            assert_eq!(t.feed(osc(case).as_bytes()), None, "{case:?} 不该被当成目录");
        }
    }

    #[test]
    fn root_dir_is_reported_once_and_not_stripped_to_empty() {
        let mut t = CwdTracker::new();
        assert_eq!(t.feed(osc("7;file://h/").as_bytes()).as_deref(), Some("/"));
        assert_eq!(t.feed(osc("7;file://h/").as_bytes()), None, "同一目录不重复上报");
        assert_eq!(t.feed(osc("7;file://h/var").as_bytes()).as_deref(), Some("/var"));
    }

    #[test]
    fn percent_escapes_are_decoded() {
        let mut t = CwdTracker::new();
        assert_eq!(
            t.feed(osc("7;file://h/home/my%20user/logs").as_bytes()).as_deref(),
            Some("/home/my user/logs")
        );
        let mut odd = CwdTracker::new();
        // 非法转义按字面留着：`100%` 这样的目录名不能被丢掉
        assert_eq!(odd.feed(osc("7;file://h/a/100%zz").as_bytes()).as_deref(), Some("/a/100%zz"));
        let mut tail = CwdTracker::new();
        assert_eq!(tail.feed(osc("7;file://h/a%").as_bytes()).as_deref(), Some("/a%"));
    }

    #[test]
    fn oversize_payload_resyncs_instead_of_growing() {
        let mut t = CwdTracker::new();
        let junk = format!("\x1b]7;file://h/{}", "a".repeat(MAX_PAYLOAD + 100));
        assert_eq!(t.feed(junk.as_bytes()), None);
        assert_eq!(t.payload.len(), 0, "超限后必须清空缓冲，不能留着继续涨");
        // 丢弃后必须能重新同步
        assert_eq!(t.feed(osc("7;file://h/y").as_bytes()).as_deref(), Some("/y"));
    }

    #[test]
    fn stray_escapes_do_not_swallow_later_sequences() {
        let mut t = CwdTracker::new();
        // CSI 噪声 + 一个 OSC 负载里出现裸 ESC 的坏序列，之后的正常序列仍要生效
        assert_eq!(t.feed(b"\x1b[31m\x1b]7;file://h/bad\x1bA"), None);
        assert_eq!(t.feed(osc("7;file://h/good").as_bytes()).as_deref(), Some("/good"));
    }
}
