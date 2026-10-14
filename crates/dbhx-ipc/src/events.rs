//! 后端主动推给渲染层的事件。
//!
//! 事件名走 tauri-specta 的 plugin 规则：`Event::NAME` 是 kebab-case，
//! `mount_events` 之后运行时名字是 `plugin:dbhx:<name>`。这就是「`dbhx/` 命名空间」的落地形式
//! —— 由框架统一加前缀，既不会撞上 Tauri 保留的 `tauri://`，也不会和别的 plugin 混淆。
//! 前端一律用生成的 `commands.$listen…`，不手写字符串。
//!
//! 三条边界：
//! 1. 载荷里只有 `SessionId` / `connection` 这类引用，**永远不放凭据、不放 endpoint 之外的敏感串**；
//!    终端字节走 [`crate::bytes::ByteBlob`]，正文不进 `Debug`。
//! 2. 每个事件都是独立类型，不是一个带 tag 的大 enum——这样 `$listen` 才能给每个事件生成
//!    精确的 TS 类型，前端不会收到「理论上可能是任何东西」的载荷。
//! 3. [`BusEvent`] 只是内部总线的一种形状，路由任务把它摊平回 `app.emit`。
//!    队列只有一个消费者（路由任务），所以事件顺序与产生顺序一致，终端输出不会错位。

use dbhx_model::{DirListing, KnownHost, LiveEndpoint, SessionId, TransferProgress};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

/// 会话输出。一个事件对应一段字节，不保证是完整的行或完整的转义序列——
/// xterm.js 的 `write` 本来就按流解析，切在哪里都不影响渲染。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct TermData {
    pub session: SessionId,
    pub data: crate::bytes::ByteBlob,
}

/// shell 报了当前目录（OSC 7）。文件面板据此决定是否跟随。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct SessionCwd {
    pub session: SessionId,
    pub path: String,
}

/// 会话结束。`exit_status` 与 `signal` 可能同时为 `None`：通道被直接关掉或整条连接断掉。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct SessionEnded {
    pub session: SessionId,
    pub exit_status: Option<u32>,
    pub signal: Option<String>,
}

/// 日志停了，但不是用户按的停。
///
/// 只有「写不下去」才发这一条：用户手动停止由命令的返回值交代，会话结束则整个
/// 标签都要消失，发事件只是多此一举。前端据此把「正在记录」的指示灯收回，
/// 否则它会一直亮着骗人。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct SessionLogStopped {
    pub session: SessionId,
    pub path: String,
    pub reason: String,
}

/// 首次见到这台主机的工作密钥，等用户确认。
///
/// `request` 是回话句柄：用户点「信任并保存」要把它带回 `hostkey_reply`。
/// 只有 `Unknown` 走这条路径——`Changed` 是硬阻断，不给按钮。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct HostKeyPending {
    pub request: u32,
    pub endpoint: LiveEndpoint,
    pub presented: KnownHost,
}

/// 主机密钥与已存记录不一致。纯通知，没有「仍然连接」的回复通道。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct HostKeyConflict {
    pub endpoint: LiveEndpoint,
    pub stored: KnownHost,
    pub presented: KnownHost,
}

/// 私钥有口令保护但 vault 里没存口令。只给身份名，不给密钥内容。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct PassphraseRequest {
    pub request: u32,
    pub identity_label: String,
}

/// 面板当前目录变了（可能来自跟随，也可能来自用户点击后的回显）。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct SftpCwdChanged {
    pub connection: u32,
    pub path: String,
}

/// 跟随被门控挡下时给前端的解释。前端据此显示「终端目录不可访问，已停在这里」这类提示，
/// 而不是让面板莫名其妙地不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FollowHold {
    NothingReported,
    UserLocked,
    Busy,
    Editing,
    Unreachable,
    AlreadyShown,
    StaleGeneration,
    WeakerSource,
}

/// 目录跟随的结果：要么导航到了新目录（带列表），要么停在原处（带原因）。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct SftpFollowed {
    pub connection: u32,
    /// `None` 表示没有导航，`hold` 说明为什么
    pub listing: Option<DirListing>,
    pub hold: Option<FollowHold>,
}

/// 批量传输进度。复用 `dbhx-model` 的形状，这里只做事件类型的外壳。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
#[tauri_specta(event_name = "transfer-progress")]
#[serde(transparent)]
pub struct TransferProgressEvent(pub TransferProgress);

/// 一批传输收尾。
///
/// 逐文件的进度走 [`TransferProgressEvent`]，但前端要能把还挂在「传输中」的条目收掉、
/// 并知道这批一共成了几个、败了几个，所以批次本身也要有一个终态事件。
/// `error` 只在整批失败（通道断了）时非空，单文件失败已经在进度里报过。
#[derive(Debug, Clone, Serialize, Deserialize, Type, Event)]
pub struct TransferBatchEnded {
    pub batch: u32,
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub bytes: u64,
    pub error: Option<String>,
}

/// vault 变了，前端重取主机列表。计数不是秘密，可以直接用来更新侧栏徽章。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type, Event)]
pub struct VaultChanged {
    pub hosts: u32,
    pub secrets: u32,
}

/// 内部总线的事件。路由任务负责摊平成上面的具体类型再 `emit`。
#[derive(Debug, Clone)]
pub enum BusEvent {
    TermData(TermData),
    SessionCwd(SessionCwd),
    SessionEnded(SessionEnded),
    SessionLogStopped(SessionLogStopped),
    HostKeyPending(HostKeyPending),
    HostKeyConflict(HostKeyConflict),
    PassphraseRequest(PassphraseRequest),
    SftpCwdChanged(SftpCwdChanged),
    SftpFollowed(SftpFollowed),
    TransferProgress(TransferProgressEvent),
    TransferBatchEnded(TransferBatchEnded),
    VaultChanged(VaultChanged),
}

impl BusEvent {
    /// 事件名（`Event::NAME` 形式）。测试用它断言「发的是哪件事」，
    /// 运行时真正用到的名字由 `mount_events` 加 plugin 前缀。
    pub fn name(&self) -> &'static str {
        match self {
            Self::TermData(_) => TermData::NAME,
            Self::SessionCwd(_) => SessionCwd::NAME,
            Self::SessionEnded(_) => SessionEnded::NAME,
            Self::SessionLogStopped(_) => SessionLogStopped::NAME,
            Self::HostKeyPending(_) => HostKeyPending::NAME,
            Self::HostKeyConflict(_) => HostKeyConflict::NAME,
            Self::PassphraseRequest(_) => PassphraseRequest::NAME,
            Self::SftpCwdChanged(_) => SftpCwdChanged::NAME,
            Self::SftpFollowed(_) => SftpFollowed::NAME,
            Self::TransferProgress(_) => TransferProgressEvent::NAME,
            Self::TransferBatchEnded(_) => TransferBatchEnded::NAME,
            Self::VaultChanged(_) => VaultChanged::NAME,
        }
    }

    pub fn dispatch(self, app: &tauri::AppHandle) {
        // emit 失败通常是 webview 已经关了；这里没有可补救的动作，也不能把路由任务带崩
        let result = match self {
            Self::TermData(e) => e.emit(app),
            Self::SessionCwd(e) => e.emit(app),
            Self::SessionEnded(e) => e.emit(app),
            Self::SessionLogStopped(e) => e.emit(app),
            Self::HostKeyPending(e) => e.emit(app),
            Self::HostKeyConflict(e) => e.emit(app),
            Self::PassphraseRequest(e) => e.emit(app),
            Self::SftpCwdChanged(e) => e.emit(app),
            Self::SftpFollowed(e) => e.emit(app),
            Self::TransferProgress(e) => e.emit(app),
            Self::TransferBatchEnded(e) => e.emit(app),
            Self::VaultChanged(e) => e.emit(app),
        };
        if let Err(e) = result {
            tracing::debug!(?e, "事件投递失败");
        }
    }
}

/// [`crate::follow_hold`]：`dbhx-sftp` 的 `Hold` ⇒ 事件里的 `FollowHold`。
///
/// 两边的变体必须一一对应，所以这里**不写通配分支**：`Hold` 加变体时这里编译失败，
/// 而不是悄悄把新原因显示成旧的。
pub fn hold_of(hold: dbhx_sftp::Hold) -> Option<FollowHold> {
    use dbhx_sftp::Hold;
    Some(match hold {
        Hold::NothingReported => FollowHold::NothingReported,
        Hold::UserLocked => FollowHold::UserLocked,
        Hold::Busy => FollowHold::Busy,
        Hold::Editing => FollowHold::Editing,
        Hold::Unreachable => FollowHold::Unreachable,
        Hold::AlreadyShown => FollowHold::AlreadyShown,
        Hold::StaleGeneration => FollowHold::StaleGeneration,
        Hold::WeakerSource => FollowHold::WeakerSource,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_sftp::Hold;

    #[test]
    fn event_names_are_kebab_and_unique() {
        let names = [
            TermData::NAME,
            SessionCwd::NAME,
            SessionEnded::NAME,
            HostKeyPending::NAME,
            HostKeyConflict::NAME,
            PassphraseRequest::NAME,
            SftpCwdChanged::NAME,
            SftpFollowed::NAME,
            TransferProgressEvent::NAME,
            TransferBatchEnded::NAME,
            VaultChanged::NAME,
        ];
        assert_eq!(names[0], "term-data", "{:?}", names);
        // 重名的话两个事件会互相覆盖监听，而 TS 侧完全看不出来
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "事件名重复：{names:?}");
    }

    #[test]
    fn every_bus_variant_names_itself() {
        // 路由任务靠 name() 决定 emit 到哪，漏一个分支就是悄悄丢事件
        let cases = [
            BusEvent::TermData(TermData { session: SessionId(1), data: Vec::<u8>::new().into() }),
            BusEvent::SessionCwd(SessionCwd { session: SessionId(1), path: "/tmp".into() }),
            BusEvent::SessionEnded(SessionEnded { session: SessionId(1), exit_status: Some(0), signal: None }),
            BusEvent::PassphraseRequest(PassphraseRequest { request: 1, identity_label: "k".into() }),
            BusEvent::SftpCwdChanged(SftpCwdChanged { connection: 1, path: "/tmp".into() }),
            BusEvent::SftpFollowed(SftpFollowed { connection: 1, listing: None, hold: Some(FollowHold::Busy) }),
            BusEvent::VaultChanged(VaultChanged { hosts: 1, secrets: 0 }),
        ];
        for case in cases {
            let name = case.name();
            assert!(name.starts_with(|c: char| c.is_ascii_lowercase()), "{name}");
            assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()), "{name}");
        }
    }

    #[test]
    fn term_data_serializes_bytes_as_base64_string() {
        let e = BusEvent::TermData(TermData { session: SessionId(3), data: vec![0x1b, 0x5b, 0x41].into() });
        let payload = serde_json::to_string(&match &e {
            BusEvent::TermData(d) => d,
            _ => unreachable!(),
        })
        .unwrap();
        // `ESC [ A` 是「上箭头」，base64 参考值用 `base64` 命令与 python 各核过一次
        assert_eq!(payload, r#"{"session":3,"data":"G1tB"}"#, "{payload}");
        assert_eq!(e.name(), "term-data");
    }

    #[test]
    fn every_hold_maps_to_a_follow_reason() {
        // 全展开而不是循环：新增 Hold 变体时 hold_of 会编译失败，这里只保证不丢
        for hold in [
            Hold::NothingReported,
            Hold::UserLocked,
            Hold::Busy,
            Hold::Editing,
            Hold::Unreachable,
            Hold::AlreadyShown,
            Hold::StaleGeneration,
            Hold::WeakerSource,
        ] {
            assert_eq!(hold_of(hold), match hold {
                Hold::NothingReported => Some(FollowHold::NothingReported),
                Hold::UserLocked => Some(FollowHold::UserLocked),
                Hold::Busy => Some(FollowHold::Busy),
                Hold::Editing => Some(FollowHold::Editing),
                Hold::Unreachable => Some(FollowHold::Unreachable),
                Hold::AlreadyShown => Some(FollowHold::AlreadyShown),
                Hold::StaleGeneration => Some(FollowHold::StaleGeneration),
                Hold::WeakerSource => Some(FollowHold::WeakerSource),
            });
        }
    }

    #[test]
    fn followed_event_carries_either_a_listing_or_a_reason() {
        let navigated = SftpFollowed {
            connection: 7,
            listing: Some(DirListing { path: "/var/log".into(), entries: vec![] }),
            hold: None,
        };
        let json = serde_json::to_string(&navigated).unwrap();
        assert!(json.contains("\"listing\""), "{json}");
        let held = SftpFollowed { connection: 7, listing: None, hold: Some(FollowHold::Busy) };
        assert!(serde_json::to_string(&held).unwrap().contains("\"busy\""));
    }

    #[test]
    fn passphrase_request_names_the_identity_not_the_key() {
        let e = PassphraseRequest { request: 1, identity_label: "运维跳板".into() };
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("BEGIN"), "{json}");
        assert!(json.contains("运维跳板"), "{json}");
    }
}
