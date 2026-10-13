//! 终端目录 → 文件面板的跟随门控。
//!
//! 这是整个 SFTP 面板里最容易把用户惹烦的地方：终端每敲一次 `cd`，右侧面板就跳一次，
//! 用户刚手动点开的目录、正在填的文件名、正在跑的任务全被打断。参考实现被这些坑磨过，
//! 规则原样保留：
//!
//! 1. **目录来源有优先级**：OSC 7 > 命令后 `pwd` 探测 > `realpath(".")` > 恢复快照。
//!    弱来源不能覆盖强来源——探测拿到的是命令执行完那一刻的目录，
//!    而 OSC 7 是 shell 自己报的，更近。
//! 2. **隐藏 tab 会送来合成的「没有目录」**（渲染层切走时补的那一下）。
//!    它绝不能作废已记录的终端目录，更不能触发跳转。这是参考实现专门写明的一处 bug。
//! 3. **有活跃传输、正在编辑路径时不跳**。等条件解除后按最后一次已知目录补跳，
//!    而不是丢弃——用户事后看到的就该是终端现在所在的位置。
//! 4. **某个目录在 SFTP 侧不可达就按 `(连接, 目录)` 拉黑**，不再反复尝试，
//!    否则每次 `cd` 都会撞一次超时。黑名单由上层持有，同一主机的多个面板共享它。
//! 5. **面板重新挂载有代数（generation）守卫**：切 tab 时上一代还在途的探测应答
//!    不能算到新一代头上。
//!
//! 类型是纯同步的，不碰异步也不碰网络——门控的全部价值就在于这些边界能被穷举测试。

use std::collections::HashSet;

use crate::path;

/// 目录信息的来源，序号越小越可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdSource {
    /// shell 通过 OSC 7 主动报出
    Osc7,
    /// 命令执行后追加的 `pwd` 探测
    Probe,
    /// 面板自己 `realpath(".")`
    Realpath,
    /// 上一次会话留下的快照（会话恢复用）
    Snapshot,
}

impl CwdSource {
    fn rank(self) -> u8 {
        match self {
            Self::Osc7 => 0,
            Self::Probe => 1,
            Self::Realpath => 2,
            Self::Snapshot => 3,
        }
    }

    /// 弱来源不得覆盖强来源。
    pub fn stronger_than(self, other: Self) -> bool {
        self.rank() < other.rank()
    }
}

/// 一个面板的跟随状态。
///
/// 它属于某一个面板，由上层（IPC 层那张面板表）持有并单线程访问；
/// 黑名单则跨面板共享，所以每个可能产生结论的方法都要显式把它传进来——
/// 门控自己不存黑名单，就不会出现「两个面板各拉黑各的」。
#[derive(Debug)]
pub struct FollowGate {
    /// 会话 id。只用于拼黑名单的键，不涉及任何 SSH 判断。
    connection: u32,
    /// 面板当前展示的目录
    shown: Option<String>,
    /// 终端最后报出的目录及其来源
    terminal: Option<(CwdSource, String)>,
    /// 用户接管（手动导航过，或关掉了「跟随终端」开关）
    user_locked: bool,
    busy: bool,
    editing: bool,
    generation: u64,
    /// 本代是否已经发过 `pwd` 探测，防止每次输出都补一条命令
    probe_sent: bool,
}

/// 门控的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 面板应当导航到该目录
    Navigate(String),
    /// 保持当前目录
    Hold(Hold),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// 没有任何终端目录可跟
    NothingReported,
    /// 用户手动浏览过，或「跟随终端」是关的
    UserLocked,
    /// 有活跃传输
    Busy,
    /// 用户正在编辑路径栏/文件名
    Editing,
    /// 该目录已被拉黑
    Unreachable,
    /// 面板已经在目标目录上
    AlreadyShown,
    /// 上一代面板的在途应答，不认
    StaleGeneration,
    /// 来源比现有记录弱，不认
    WeakerSource,
}

impl FollowGate {
    pub fn new(connection: u32) -> Self {
        Self {
            connection,
            shown: None,
            terminal: None,
            user_locked: false,
            busy: false,
            editing: false,
            generation: 0,
            probe_sent: false,
        }
    }

    pub fn connection(&self) -> u32 {
        self.connection
    }

    pub fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    pub fn terminal_cwd(&self) -> Option<&str> {
        self.terminal.as_ref().map(|(_, path)| path.as_str())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 面板重新挂载（切 tab、首次打开）。代数 +1：上一代在途的应答从此不认，
    /// 探测也允许重发一次。
    pub fn renew(&mut self) -> u64 {
        self.generation += 1;
        self.probe_sent = false;
        self.generation
    }

    /// 用户手动点了目录：立刻认用户的，之后不再被终端拽走，直到 [`Self::set_following`]。
    /// 终端目录仍然照常记录，重新打开跟随时会补跳过去。
    pub fn user_browsed(&mut self, target: &str) {
        self.shown = Some(path::normalize(target));
        self.user_locked = true;
    }

    /// 「跟随终端」开关。关→开时按现有状态重判一次，可能马上补跳。
    pub fn set_following(&mut self, on: bool, blacklist: &Blacklist) -> Decision {
        self.user_locked = !on;
        self.evaluate(blacklist)
    }

    /// 传输开始/结束。结束时要补上期间被压住的那次跳转。
    pub fn set_busy(&mut self, busy: bool, blacklist: &Blacklist) -> Decision {
        self.busy = busy;
        self.evaluate(blacklist)
    }

    /// 路径栏/重命名输入框的焦点。失焦时同样补跳。
    pub fn set_editing(&mut self, editing: bool, blacklist: &Blacklist) -> Decision {
        self.editing = editing;
        self.evaluate(blacklist)
    }

    /// 本代是否还需要发一次 `pwd` 探测。
    ///
    /// OSC 7 已经报过就不探测——它比探测准，而且探测会在终端里留下一条命令痕迹。
    pub fn should_probe(&self) -> bool {
        !self.probe_sent
            && match self.terminal.as_ref() {
                None => true,
                Some((source, _)) => *source != CwdSource::Osc7,
            }
    }

    /// 记下「这代已经探测过了」。
    pub fn mark_probe_sent(&mut self) {
        self.probe_sent = true;
    }

    /// 收到终端目录事件。
    ///
    /// `cwd` 为 `None` 代表「这个 tab 现在没有目录可报」（隐藏 tab 的合成事件）：
    /// 保持原样，既不清空已记录的终端目录，也不触发跳转。
    pub fn report(&mut self, generation: u64, cwd: Option<&str>, source: CwdSource, blacklist: &Blacklist) -> Decision {
        if generation != self.generation {
            return Decision::Hold(Hold::StaleGeneration);
        }
        let Some(cwd) = cwd else {
            return Decision::Hold(Hold::NothingReported);
        };
        if let Some((known_source, _)) = self.terminal.as_ref() {
            // 已有记录比这次更可信：这次是弱来源，不认
            if known_source.stronger_than(source) {
                return Decision::Hold(Hold::WeakerSource);
            }
        }
        // 同级（下一次 `cd` 的 OSC 7）与更强来源都覆盖：那就是新位置。
        // 记录前先归一化，`/var/log` 与 `/var/log/` 是同一个目录，
        // 不统一就会让面板对同一次 cd 重画一遍。
        self.terminal = Some((source, path::normalize(cwd)));
        self.evaluate(blacklist)
    }

    /// 目录在 SFTP 侧不可达：拉黑并保持现状。
    ///
    /// 连当前记录一起丢掉——不然解除 busy 之后又会拿同一个进不去的目录再撞一次。
    pub fn mark_unreachable(&mut self, blacklist: &mut Blacklist, target: &str) -> Decision {
        blacklist.add(self.connection, target);
        let normalized = path::normalize(target);
        if self.terminal.as_ref().is_some_and(|(_, known)| *known == normalized) {
            self.terminal = None;
        }
        self.evaluate(blacklist)
    }

    /// 按当前状态求值。上层在改完外部条件（黑名单变化等）后可以用它重判。
    pub fn evaluate(&self, blacklist: &Blacklist) -> Decision {
        let Some((_, target)) = self.terminal.as_ref() else {
            return Decision::Hold(Hold::NothingReported);
        };
        if self.user_locked {
            return Decision::Hold(Hold::UserLocked);
        }
        if self.busy {
            return Decision::Hold(Hold::Busy);
        }
        if self.editing {
            return Decision::Hold(Hold::Editing);
        }
        if blacklist.contains(self.connection, target) {
            return Decision::Hold(Hold::Unreachable);
        }
        if self.shown.as_deref() == Some(target.as_str()) {
            return Decision::Hold(Hold::AlreadyShown);
        }
        Decision::Navigate(target.clone())
    }

    /// 门控给出 `Navigate` 且面板真的跳过去了，由这里回填「已展示」。
    ///
    /// 单独一步是因为跳转可能失败：失败时不该改 `shown`，
    /// 上层应当改调 [`Self::mark_unreachable`]。
    pub fn confirm_navigated(&mut self, target: &str) {
        self.shown = Some(path::normalize(target));
    }
}

/// `(连接, 目录)` 不可达黑名单。
///
/// 独立成类型而不是塞进门控，是因为同一台主机的多个面板要共享这份记录：
/// 一个面板已经证明进不去的目录，另一个面板没必要再试一次。
#[derive(Debug, Default)]
pub struct Blacklist {
    entries: HashSet<(u32, String)>,
}

impl Blacklist {
    pub fn add(&mut self, connection: u32, target: &str) {
        self.entries.insert((connection, path::normalize(target)));
    }

    pub fn contains(&self, connection: u32, target: &str) -> bool {
        self.entries.contains(&(connection, path::normalize(target)))
    }

    /// 连接断开时清掉它的全部记录：重连之后同一目录可能又可访问了，
    /// 留着会让面板永远不再跟随。
    pub fn clear_connection(&mut self, connection: u32) {
        self.entries.retain(|(id, _)| *id != connection);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 约定：会话 1 的终端在 `/var/log`，面板还没跳。
    fn gate_at_shown(shown: Option<&str>) -> FollowGate {
        let mut gate = FollowGate::new(1);
        gate.shown = shown.map(str::to_string);
        gate
    }

    #[test]
    fn a_fresh_report_moves_the_panel() {
        let mut gate = gate_at_shown(Some("/etc"));
        let blacklist = Blacklist::default();
        let d = gate.report(0, Some("/var/log"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Navigate("/var/log".into()));
        gate.confirm_navigated("/var/log");
        assert_eq!(gate.shown(), Some("/var/log"));
    }

    #[test]
    fn the_same_directory_spelled_differently_does_not_move_twice() {
        let mut gate = gate_at_shown(Some("/var/log"));
        let blacklist = Blacklist::default();
        let d = gate.report(0, Some("/var/log/"), CwdSource::Osc7, &blacklist);
        assert_eq!(gate.terminal_cwd(), Some("/var/log"), "尾斜杠是写法差异，记录前先归一化");
        assert_eq!(d, Decision::Hold(Hold::AlreadyShown));
    }

    #[test]
    fn hidden_tab_null_report_changes_nothing() {
        // 参考实现专门写明的 bug：切 tab 时渲染层会补一个 null，
        // 若据此作废记录，用户手动浏览的目录就会被拽回终端 cwd
        let mut gate = gate_at_shown(Some("/etc"));
        let blacklist = Blacklist::default();
        gate.report(0, Some("/var/log"), CwdSource::Osc7, &blacklist);
        let d = gate.report(0, None, CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::NothingReported));
        assert_eq!(gate.terminal_cwd(), Some("/var/log"), "null 不许清空已记录的目录");
    }

    #[test]
    fn weaker_source_cannot_override_osc7() {
        let mut gate = gate_at_shown(Some("/etc"));
        let blacklist = Blacklist::default();
        gate.report(0, Some("/var/log"), CwdSource::Osc7, &blacklist);
        let d = gate.report(0, Some("/home/app"), CwdSource::Probe, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::WeakerSource));
        assert_eq!(gate.terminal_cwd(), Some("/var/log"));
    }

    #[test]
    fn stronger_source_overrides_and_equal_source_advances() {
        let mut gate = gate_at_shown(Some("/etc"));
        let blacklist = Blacklist::default();
        gate.report(0, Some("/tmp"), CwdSource::Probe, &blacklist);
        let d = gate.report(0, Some("/var/log"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Navigate("/var/log".into()), "OSC 7 覆盖探测");
        // 同为 OSC 7 的下一次 cd 是「新位置」，必须跟
        gate.confirm_navigated("/var/log");
        let d = gate.report(0, Some("/var"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Navigate("/var".into()));
    }

    #[test]
    fn transfer_holds_the_jump_then_fires_it_once() {
        let mut gate = gate_at_shown(Some("/etc"));
        let mut blacklist = Blacklist::default();
        gate.set_busy(true, &blacklist);
        let d = gate.report(0, Some("/var/log"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::Busy));
        // 期间用户又 cd 了一次：只补跳到最新的那个
        gate.report(0, Some("/data"), CwdSource::Osc7, &blacklist);
        let d = gate.set_busy(false, &blacklist);
        assert_eq!(d, Decision::Navigate("/data".into()));
        gate.confirm_navigated("/data");
        let d = gate.set_busy(false, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::AlreadyShown), "补跳只做一次");
        blacklist.clear_connection(1);
    }

    #[test]
    fn editing_holds_but_user_browsing_locks_until_reenabled() {
        let mut gate = gate_at_shown(Some("/etc"));
        let blacklist = Blacklist::default();
        gate.set_editing(true, &blacklist);
        let d = gate.report(0, Some("/var/log"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::Editing), "用户正在输入路径时不能把地址栏换掉");
        let d = gate.set_editing(false, &blacklist);
        assert_eq!(d, Decision::Navigate("/var/log".into()), "输入结束后补跳");

        // 用户手动点开 /srv，之后再 cd 不该动
        gate.confirm_navigated("/var/log");
        gate.user_browsed("/srv/");
        assert_eq!(gate.shown(), Some("/srv"));
        let d = gate.report(0, Some("/home/app"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::UserLocked));
        assert_eq!(gate.terminal_cwd(), Some("/home/app"), "用户接管期间仍要记账");
        let d = gate.set_following(true, &blacklist);
        assert_eq!(d, Decision::Navigate("/home/app".into()));
    }

    #[test]
    fn unreachable_directory_is_blacklisted_and_dropped() {
        let mut gate = gate_at_shown(Some("/etc"));
        let mut blacklist = Blacklist::default();
        gate.report(0, Some("/mnt/nfs"), CwdSource::Osc7, &blacklist);
        let d = gate.mark_unreachable(&mut blacklist, "/mnt/nfs");
        assert_eq!(d, Decision::Hold(Hold::NothingReported), "拉黑后不再有想去的目标");
        // 终端再次报同一个目录：黑名单挡住了，不会又给一次 Navigate
        let d = gate.report(0, Some("/mnt/nfs"), CwdSource::Osc7, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::Unreachable));
        assert_eq!(blacklist.len(), 1);
    }

    #[test]
    fn blacklist_key_ignores_spelling_and_other_connections() {
        let mut blacklist = Blacklist::default();
        blacklist.add(1, "/var/log/");
        assert!(blacklist.contains(1, "/var/log"));
        assert!(!blacklist.contains(2, "/var/log"), "另一条连接上同一个目录可能可访问");
        blacklist.clear_connection(1);
        assert!(blacklist.is_empty());
    }

    #[test]
    fn stale_generation_is_rejected_without_touching_state() {
        let mut gate = gate_at_shown(Some("/etc"));
        let blacklist = Blacklist::default();
        let gen = gate.renew();
        assert_eq!(gen, 1);
        let d = gate.report(0, Some("/old"), CwdSource::Probe, &blacklist);
        assert_eq!(d, Decision::Hold(Hold::StaleGeneration));
        assert_eq!(gate.terminal_cwd(), None);
        assert!(gate.should_probe(), "新一代允许重新探测一次");
    }

    #[test]
    fn probe_is_offered_only_until_osc7_speaks() {
        let mut gate = FollowGate::new(1);
        let blacklist = Blacklist::default();
        assert!(gate.should_probe());
        gate.mark_probe_sent();
        assert!(!gate.should_probe(), "同一代不重复探测");
        gate.report(gate.generation(), Some("/var"), CwdSource::Probe, &blacklist);
        assert!(!gate.should_probe(), "探测已经给出答案");
        gate.renew();
        assert!(gate.should_probe(), "OSC 7 还没报过，新一代可以探");
        gate.report(gate.generation(), Some("/var/log"), CwdSource::Osc7, &blacklist);
        assert!(!gate.should_probe(), "OSC 7 报过就不再探测");
    }
}
