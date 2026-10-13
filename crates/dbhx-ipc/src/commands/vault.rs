//! 主机库与信任列表命令。
//!
//! 两条硬规则写在这里：
//!
//! 1. **`SecretId` 不出 IPC**。vault 的内部记录（`HostRecord`/`IdentityRecord`）带着
//!    密文条目的引用，渲染层拿到它就能指名换出别的记录的口令，所以命令层用两套
//!    「草案 / 投影」类型（[`HostDraft`]、[`IdentityInfo`]），`SecretId` 只在 Rust 内部存在。
//! 2. **口令只进不出**。`vault_set_password` 的入参是用户刚敲的明文（这是唯一允许明文
//!    跨界的方——它来自渲染层的输入框），立刻封进 `Zeroizing` 交给 vault；
//!    任何命令的返回值里都不会再有口令字段。
//!
//! vault 与 known_hosts 都是「改完即落盘」：进程崩了不该丢用户刚存的主机。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use dbhx_model::{
    AuthMode, Endpoint, ErrorCode, HostId, HostInfo, HostRecord, IpcError, IdentityId, KnownHost, Protocol,
    TerminalEncoding, now_ms,
};
use dbhx_vault::SecretKind;
use serde::{Deserialize, Serialize};
use specta::Type;
use zeroize::Zeroizing;

use crate::events::{BusEvent, VaultChanged};
use crate::state::Core;

/// 前端能提交的主机形状：与 `HostRecord` 同字段，但没有任何密文引用。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct HostDraft {
    /// `None` 表示新建，由 vault 分配 id
    pub id: Option<HostId>,
    pub label: String,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub favorite: bool,
    pub protocol: Protocol,
    pub endpoint: Endpoint,
    pub auth_mode: AuthMode,
    pub identity_id: Option<IdentityId>,
    pub keepalive_secs: u16,
    pub encoding: TerminalEncoding,
}

/// 一条身份的可外显信息。私钥内容、密文引用都不在这里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct IdentityInfo {
    pub id: IdentityId,
    pub label: String,
    /// vault 里是否存了口令。没有的话，连接时才需要问用户（见 `state::build_plan`）
    pub has_passphrase: bool,
    /// 密钥是 PEM 还是 PuTTY 格式，纯展示用
    pub is_ppk: bool,
}

/// 用户对后端询问的回答。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UserReply {
    Confirmed,
    Cancelled,
    Secret { text: String },
}

/// 应用锁状态。锁屏覆盖层与设置抽屉都只按这两个布尔分支，拿不到 PIN 本身。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct LockStatus {
    /// 当前是否处于锁定态（锁定后禁止新拨号与凭据读写）
    pub locked: bool,
    /// 是否设置过解锁 PIN。没设过就不可能进入锁定态，覆盖层据此决定显不显示。
    pub has_pin: bool,
}

/// 草案 + 既有记录 ⇒ 落盘记录。**已有口令引用必须原样保留**，
/// 否则「改个分组名」就会把凭据清空。
pub fn merge_record(draft: HostDraft, existing: Option<&HostRecord>, now: u64) -> HostRecord {
    let id = draft.id.clone().or_else(|| existing.map(|e| e.id.clone())).unwrap_or_else(|| HostId(String::new()));
    HostRecord {
        id,
        label: draft.label,
        group: draft.group,
        tags: draft.tags,
        favorite: draft.favorite,
        protocol: draft.protocol,
        endpoint: draft.endpoint,
        auth_mode: draft.auth_mode,
        identity_id: draft.identity_id,
        password_secret_id: existing.and_then(|e| e.password_secret_id.clone()),
        keepalive_secs: draft.keepalive_secs,
        encoding: draft.encoding,
        created_at_ms: existing.map(|e| e.created_at_ms).unwrap_or(now),
        updated_at_ms: now,
    }
}

pub fn hosts_impl(core: &Core) -> Result<Vec<HostInfo>, IpcError> {
    let vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    Ok(vault.hosts().iter().map(HostInfo::from_record).collect())
}

pub fn upsert_host_impl(core: &Core, draft: HostDraft) -> Result<HostId, IpcError> {
    reject_empty(&draft.label, "主机名不能为空")?;
    reject_empty(&draft.endpoint.username, "登录用户名不能为空")?;
    let now = now_ms();
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    let existing = draft.id.as_ref().and_then(|id| vault.host(id)).cloned();
    if existing.is_none() && draft.id.is_some() {
        return Err(IpcError::new(ErrorCode::NotFound, "要修改的主机记录已不存在", false));
    }
    let record = merge_record(draft, existing.as_ref(), now);
    let id = vault.upsert_host(record);
    vault.flush()?;
    emit_changed(core, &vault);
    Ok(id)
}

pub fn remove_host_impl(core: &Core, id: HostId) -> Result<(), IpcError> {
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    vault.remove_host(&id)?;
    vault.flush()?;
    emit_changed(core, &vault);
    Ok(())
}

/// 存主机口令。明文只活在这一行：进 `Zeroizing` 后立刻交给 vault 加密。
pub fn set_password_impl(core: &Core, id: HostId, password: Zeroizing<String>) -> Result<(), IpcError> {
    core.require_unlocked()?;
    if password.is_empty() {
        return Err(IpcError::new(ErrorCode::InvalidInput, "口令不能为空", false));
    }
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    vault.set_host_password(&id, password.as_bytes())?;
    vault.flush()?;
    emit_changed(core, &vault);
    Ok(())
}

pub fn identities_impl(core: &Core) -> Result<Vec<IdentityInfo>, IpcError> {
    let vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    Ok(vault.identities().iter().map(|i| identity_info(&vault, i)).collect())
}

/// 投影一条身份：只问 vault「有没有口令、是不是 ppk」，不把密文引用带出去。
fn identity_info(vault: &dbhx_vault::Vault, identity: &dbhx_model::IdentityRecord) -> IdentityInfo {
    let is_ppk = vault
        .with_secret(&identity.private_key_secret_id, |bytes| {
            Ok(String::from_utf8_lossy(bytes).trim_start().starts_with("PuTTY-User-Key-File-"))
        })
        .unwrap_or(false);
    let has_passphrase = identity
        .passphrase_secret_id
        .as_ref()
        .map(|id| vault.secret_kind(id) == Some(SecretKind::Passphrase))
        .unwrap_or(false);
    IdentityInfo { id: identity.id.clone(), label: identity.label.clone(), has_passphrase, is_ppk }
}

pub fn add_identity_impl(
    core: &Core,
    label: String,
    private_key: Zeroizing<String>,
    passphrase: Option<Zeroizing<String>>,
) -> Result<IdentityId, IpcError> {
    core.require_unlocked()?;
    reject_empty(&label, "身份名称不能为空")?;
    if !private_key.contains("PRIVATE KEY") && !private_key.starts_with("PuTTY-User-Key-File-") {
        return Err(IpcError::new(
            ErrorCode::InvalidInput,
            "看起来不是私钥文件：既没有 PRIVATE KEY 标记，也不是 PuTTY 格式",
            false,
        ));
    }
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    let id = vault.add_identity(&label, private_key.as_bytes(), passphrase.as_ref().map(|p| p.as_bytes()))?;
    vault.flush()?;
    emit_changed(core, &vault);
    Ok(id)
}

pub fn remove_identity_impl(core: &Core, id: IdentityId) -> Result<(), IpcError> {
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    vault.remove_identity(&id)?;
    vault.flush()?;
    emit_changed(core, &vault);
    Ok(())
}

pub fn known_hosts_impl(core: &Core) -> Result<Vec<KnownHost>, IpcError> {
    let known = core.known.lock().unwrap_or_else(|p| p.into_inner());
    Ok(known.entries().to_vec())
}

pub fn forget_host_key_impl(core: &Core, host: String, port: u16, algorithm: String) -> Result<u32, IpcError> {
    let mut known = core.known.lock().unwrap_or_else(|p| p.into_inner());
    let removed = known.forget(&host, port, &algorithm) as u32;
    known.save()?;
    Ok(removed)
}

/// 只读导入系统的 `~/.ssh/known_hosts`。源文件不会被写回，理由见 `dbhx-vault::hostkey`。
pub fn import_known_hosts_impl(core: &Core) -> Result<u32, IpcError> {
    let source = default_user_known_hosts()?;
    let mut known = core.known.lock().unwrap_or_else(|p| p.into_inner());
    let imported = known.import_readonly(&source)? as u32;
    known.save()?;
    Ok(imported)
}

fn default_user_known_hosts() -> Result<std::path::PathBuf, IpcError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .ok_or_else(|| IpcError::new(ErrorCode::Io, "找不到用户目录，无法导入 known_hosts", false))?;
    Ok(home.join(".ssh").join("known_hosts"))
}

pub fn reply_impl(core: &Core, request: u32, reply: UserReply) -> Result<(), IpcError> {
    let answer = match reply {
        UserReply::Confirmed => crate::state::UserAnswer::Confirmed,
        UserReply::Cancelled => crate::state::UserAnswer::Cancelled,
        UserReply::Secret { text } => crate::state::UserAnswer::Secret(Zeroizing::new(text)),
    };
    core.awaits.reply(request, answer)
}

/// 应用锁：UI/授权门，**不是加密边界**。主密钥仍在进程里，锁的意义是「锁屏期间
/// 不给渲染层任何读凭据或发起新连接的机会」，而非抵御能读到磁盘文件的攻击者。
/// PIN 作为密封条目存在 vault 里，任何命令都不会把它投影出去——连计数之外的一切都不给。

/// 锁状态。覆盖层与设置抽屉只按 `locked`/`has_pin` 分支，拿不到 PIN。
pub fn lock_status_impl(core: &Core) -> LockStatus {
    let has_pin = core.vault.lock().unwrap_or_else(|p| p.into_inner()).has_lock_pin();
    LockStatus { locked: core.locked.load(Ordering::Relaxed), has_pin }
}

/// 进入锁定态。没设过 PIN 时拒绝——否则会把用户永久关在门外。
pub fn lock_impl(core: &Core) -> Result<(), IpcError> {
    if !core.vault.lock().unwrap_or_else(|p| p.into_inner()).has_lock_pin() {
        return Err(IpcError::new(ErrorCode::InvalidInput, "请先设置解锁 PIN 再启用应用锁", false));
    }
    core.locked.store(true, Ordering::Relaxed);
    Ok(())
}

/// 用 PIN 解锁。明文只来自渲染层输入框，立刻 `Zeroizing` 后走常数时间比对，
/// 比对结果不泄露内容。错 PIN 回 `PermissionDenied`（重试无意义，得重新输入）。
pub fn unlock_impl(core: &Core, pin: Zeroizing<String>) -> Result<(), IpcError> {
    let vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    let ok = vault
        .check_lock_pin(pin.as_bytes())
        .map_err(|_| IpcError::new(ErrorCode::Internal, "解锁校验失败，请稍后再试", true))?;
    if !ok {
        return Err(IpcError::new(ErrorCode::PermissionDenied, "PIN 不正确", false));
    }
    core.locked.store(false, Ordering::Relaxed);
    Ok(())
}

/// 设置或更换解锁 PIN。必须先处于解锁态（锁着改不了锁）。
pub fn set_lock_pin_impl(core: &Core, pin: Zeroizing<String>) -> Result<(), IpcError> {
    core.require_unlocked()?;
    let bytes = pin.as_bytes();
    if bytes.len() < MIN_PIN_LEN {
        return Err(IpcError::new(ErrorCode::InvalidInput, format!("PIN 至少 {MIN_PIN_LEN} 位"), false));
    }
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    vault.set_lock_pin(bytes)?;
    vault.flush()?;
    emit_changed(core, &vault);
    Ok(())
}

/// 移除解锁 PIN：没有 PIN 就无法成立锁定态，顺手置回解锁。
pub fn clear_lock_pin_impl(core: &Core) -> Result<(), IpcError> {
    core.require_unlocked()?;
    {
        let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
        vault.clear_lock_pin();
        vault.flush()?;
        emit_changed(core, &vault);
    }
    core.locked.store(false, Ordering::Relaxed);
    Ok(())
}

/// 解锁 PIN 的最小长度。太短的 PIN 与「没有锁」差别不大，宁可拒绝。
const MIN_PIN_LEN: usize = 4;

fn reject_empty(value: &str, message: &str) -> Result<(), IpcError> {
    if value.trim().is_empty() {
        return Err(IpcError::new(ErrorCode::InvalidInput, message, false));
    }
    Ok(())
}

fn counts_of(vault: &dbhx_vault::Vault) -> (u32, u32) {
    (vault.hosts().len() as u32, vault.secret_count() as u32)
}

/// 改完就发一次变更通知。计数不是秘密，可以直接用来更新侧栏徽章与「有几条主机」的角标。
fn emit_changed(core: &Core, vault: &dbhx_vault::Vault) {
    let (hosts, secrets) = counts_of(vault);
    core.emit(BusEvent::VaultChanged(VaultChanged { hosts, secrets }));
}

type State<'a> = tauri::State<'a, Arc<Core>>;

#[tauri::command]
#[specta::specta]
pub fn vault_hosts(core: State<'_>) -> Result<Vec<HostInfo>, IpcError> {
    hosts_impl(&core)
}

/// 新建或修改主机。**不会改动已存的口令**，改口令是另一条命令。
#[tauri::command]
#[specta::specta]
pub fn vault_upsert_host(core: State<'_>, draft: HostDraft) -> Result<HostId, IpcError> {
    upsert_host_impl(&core, draft)
}

#[tauri::command]
#[specta::specta]
pub fn vault_remove_host(core: State<'_>, host_id: HostId) -> Result<(), IpcError> {
    remove_host_impl(&core, host_id)
}

#[tauri::command]
#[specta::specta]
pub fn vault_set_password(core: State<'_>, host_id: HostId, password: String) -> Result<(), IpcError> {
    set_password_impl(&core, host_id, Zeroizing::new(password))
}

#[tauri::command]
#[specta::specta]
pub fn vault_identities(core: State<'_>) -> Result<Vec<IdentityInfo>, IpcError> {
    identities_impl(&core)
}

#[tauri::command]
#[specta::specta]
pub fn vault_add_identity(
    core: State<'_>,
    label: String,
    private_key: String,
    passphrase: Option<String>,
) -> Result<IdentityId, IpcError> {
    add_identity_impl(
        &core,
        label,
        Zeroizing::new(private_key),
        passphrase.map(Zeroizing::new),
    )
}

#[tauri::command]
#[specta::specta]
pub fn vault_remove_identity(core: State<'_>, identity_id: IdentityId) -> Result<(), IpcError> {
    remove_identity_impl(&core, identity_id)
}

#[tauri::command]
#[specta::specta]
pub fn hostkeys_list(core: State<'_>) -> Result<Vec<KnownHost>, IpcError> {
    known_hosts_impl(&core)
}

#[tauri::command]
#[specta::specta]
pub fn hostkeys_forget(core: State<'_>, host: String, port: u16, algorithm: String) -> Result<u32, IpcError> {
    forget_host_key_impl(&core, host, port, algorithm)
}

#[tauri::command]
#[specta::specta]
pub fn hostkeys_import(core: State<'_>) -> Result<u32, IpcError> {
    import_known_hosts_impl(&core)
}

/// 回答一次后端发起的询问（主机密钥确认 / 私钥口令）。
#[tauri::command]
#[specta::specta]
pub fn request_reply(core: State<'_>, request: u32, reply: UserReply) -> Result<(), IpcError> {
    reply_impl(&core, request, reply)
}

#[tauri::command]
#[specta::specta]
pub fn vault_lock_status(core: State<'_>) -> LockStatus {
    lock_status_impl(&core)
}

#[tauri::command]
#[specta::specta]
pub fn vault_lock(core: State<'_>) -> Result<(), IpcError> {
    lock_impl(&core)
}

/// 解锁：PIN 从渲染层输入框进来，是唯一允许明文跨界的又一个入口（方向朝内）。
#[tauri::command]
#[specta::specta]
pub fn vault_unlock(core: State<'_>, pin: String) -> Result<(), IpcError> {
    unlock_impl(&core, Zeroizing::new(pin))
}

#[tauri::command]
#[specta::specta]
pub fn vault_set_lock_pin(core: State<'_>, pin: String) -> Result<(), IpcError> {
    set_lock_pin_impl(&core, Zeroizing::new(pin))
}

#[tauri::command]
#[specta::specta]
pub fn vault_clear_lock_pin(core: State<'_>) -> Result<(), IpcError> {
    clear_lock_pin_impl(&core)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests_harness;
    use dbhx_model::ErrorCode;

    fn draft(label: &str) -> HostDraft {
        HostDraft {
            id: None,
            label: label.into(),
            group: Some("db".into()),
            tags: vec!["prod".into()],
            favorite: true,
            protocol: Protocol::Ssh,
            endpoint: Endpoint { host: "10.0.0.1".into(), port: Some(2222), username: "app".into() },
            auth_mode: AuthMode::Password,
            identity_id: None,
            keepalive_secs: 30,
            encoding: TerminalEncoding::Utf8,
        }
    }

    #[test]
    fn editing_a_host_keeps_the_stored_password_reference() {
        // 「只改分组」把口令弄丢是参考实现里被投诉过的行为，回归测试钉住它
        let existing = HostRecord {
            id: HostId("h1".into()),
            label: "旧名".into(),
            group: None,
            tags: vec![],
            favorite: false,
            protocol: Protocol::Ssh,
            endpoint: Endpoint { host: "h".into(), port: None, username: "u".into() },
            auth_mode: AuthMode::Password,
            identity_id: None,
            password_secret_id: Some(dbhx_model::SecretId("secret-keep".into())),
            keepalive_secs: 0,
            encoding: TerminalEncoding::Utf8,
            created_at_ms: 7,
            updated_at_ms: 8,
        };
        let merged = merge_record(HostDraft { id: Some(HostId("h1".into())), ..draft("新名") }, Some(&existing), 99);
        assert_eq!(merged.label, "新名");
        assert_eq!(merged.password_secret_id.as_ref().map(|s| s.0.as_str()), Some("secret-keep"));
        assert_eq!(merged.created_at_ms, 7, "创建时间不能被更新覆盖");
        assert_eq!(merged.updated_at_ms, 99);
    }

    #[test]
    fn a_new_host_has_no_password_reference() {
        let merged = merge_record(draft("db1"), None, 5);
        assert!(merged.password_secret_id.is_none());
        assert_eq!(merged.created_at_ms, 5);
    }

    #[tokio::test]
    async fn upsert_then_list_round_trips_and_reports_counts() {
        let (core, mut rx) = tests_harness("vault-roundtrip", crate::state::fake_dialer());
        let id = upsert_host_impl(&core, draft("db1")).unwrap();
        let hosts = hosts_impl(&core).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].id, id);
        assert_eq!(hosts[0].endpoint.port, Some(2222));
        match rx.recv().await.unwrap() {
            crate::events::BusEvent::VaultChanged(c) => {
                assert_eq!(c.hosts, 1);
                assert_eq!(c.secrets, 0, "只改记录不该多出密文条目");
            }
            other => panic!("期望 vault 变更事件，收到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_label_is_refused_before_anything_is_written() {
        let (core, _rx) = tests_harness("vault-empty", crate::state::fake_dialer());
        let err = upsert_host_impl(&core, draft("   ")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(hosts_impl(&core).unwrap().is_empty(), "校验失败不能留下半条记录");
    }

    #[tokio::test]
    async fn setting_a_password_adds_exactly_one_secret_and_it_survives_reopen() {
        let dir = {
            let (core, _rx) = tests_harness("vault-secret", crate::state::fake_dialer());
            let id = upsert_host_impl(&core, draft("db2")).unwrap();
            set_password_impl(&core, id.clone(), Zeroizing::new("hunter2".into())).unwrap();
            assert_eq!(core.vault.lock().unwrap().secret_count(), 1);
            // 明文不出这个函数：投影出来的只有引用
            let hosts = hosts_impl(&core).unwrap();
            let json = serde_json::to_string(&hosts).unwrap();
            assert!(!json.contains("hunter2"), "{json}");
            assert!(!json.contains("secret"), "{json}");
            core.dir().to_path_buf()
        };
        // 重新打开同一个目录：证明 flush 真的落盘了，而不是只在内存里改了改
        let vault = dbhx_vault::Vault::open(&dir, dbhx_vault::KeySource::File(dir.join("master.key"))).unwrap();
        assert_eq!(vault.secret_count(), 1);
        let host = vault.hosts()[0].clone();
        let secret = host.password_secret_id.clone().expect("口令引用要在");
        let plain = vault.with_secret(&secret, |b| Ok(String::from_utf8_lossy(b).into_owned())).unwrap();
        assert_eq!(plain, "hunter2");
    }

    #[tokio::test]
    async fn replacing_a_password_does_not_leave_the_old_secret_behind() {
        let (core, _rx) = tests_harness("vault-rotate", crate::state::fake_dialer());
        let id = upsert_host_impl(&core, draft("db3")).unwrap();
        set_password_impl(&core, id.clone(), Zeroizing::new("first".into())).unwrap();
        set_password_impl(&core, id.clone(), Zeroizing::new("second".into())).unwrap();
        assert_eq!(core.vault.lock().unwrap().secret_count(), 1, "旧密文没人引用就该回收");
    }

    #[tokio::test]
    async fn a_key_that_is_not_a_private_key_is_rejected() {
        let (core, _rx) = tests_harness("vault-key", crate::state::fake_dialer());
        let err = add_identity_impl(&core, "跳板".into(), Zeroizing::new("ssh-ed25519 AAAA...".into()), None).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("私钥"), "{}", err.message);
        assert_eq!(core.vault.lock().unwrap().secret_count(), 0);
    }

    #[tokio::test]
    async fn identity_projection_never_carries_a_secret_reference() {
        let (core, _rx) = tests_harness("vault-project", crate::state::fake_dialer());
        let pem = crate::test_keys::PLAIN_ED25519.to_string();
        let id = add_identity_impl(&core, "运维".into(), Zeroizing::new(pem), None).unwrap();
        let listed = identities_impl(&core).unwrap();
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].label, "运维");
        assert!(!listed[0].has_passphrase);
        assert!(!listed[0].is_ppk, "PEM 不该被认成 ppk");
        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains("secret"), "{json}");
    }

    #[tokio::test]
    async fn an_encrypted_key_projects_has_passphrase_false_until_the_user_stores_it() {
        // vault 里没存口令时，加密私钥要走「连的时候问人」那条路，见 state::build_plan
        let (core, _rx) = tests_harness("vault-enc", crate::state::fake_dialer());
        let id = add_identity_impl(&core, "加密键".into(), Zeroizing::new(crate::test_keys::ENC_ED25519.to_string()), None)
            .unwrap();
        let listed = identities_impl(&core).unwrap();
        assert_eq!(listed[0].id, id);
        assert!(!listed[0].has_passphrase, "没存口令就是没有口令");
        assert!(dbhx_ssh::key_is_encrypted(crate::test_keys::ENC_ED25519), "前提：这把测试密钥确实是加密的");
        assert!(!dbhx_ssh::key_is_encrypted(crate::test_keys::PLAIN_ED25519), "前提：另一把是明文密钥");
    }

    #[tokio::test]
    async fn a_putty_key_is_recognized_by_its_header_not_its_extension() {
        let (core, _rx) = tests_harness("vault-ppk", crate::state::fake_dialer());
        add_identity_impl(&core, "ppk".into(), Zeroizing::new(crate::test_keys::PPK_HEADER.into()), None).unwrap();
        assert!(identities_impl(&core).unwrap()[0].is_ppk);
    }

    #[tokio::test]
    async fn replying_twice_to_the_same_request_is_not_found() {
        let (core, _rx) = tests_harness("vault-reply", crate::state::fake_dialer());
        let (request, _rx_answer) = core.awaits.register();
        reply_impl(&core, request, UserReply::Confirmed).unwrap();
        let err = reply_impl(&core, request, UserReply::Secret { text: "again".into() }).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound, "第二次回答必须失败，否则口令会送进已经放弃的拨号");
    }

    #[tokio::test]
    async fn forgetting_an_unknown_host_key_reports_zero_not_an_error() {
        let (core, _rx) = tests_harness("vault-forget", crate::state::fake_dialer());
        assert_eq!(forget_host_key_impl(&core, "nope".into(), 22, "ssh-ed25519".into()).unwrap(), 0);
    }

    #[tokio::test]
    async fn locking_without_a_pin_is_refused() {
        // 没设过 PIN 就锁定会把用户永久关在门外，必须挡在建锁之前
        let (core, _rx) = tests_harness("lock-nopin", crate::state::fake_dialer());
        let err = lock_impl(&core).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(!lock_status_impl(&core).locked);
    }

    #[tokio::test]
    async fn a_pin_enables_lock_and_blocks_new_dials_and_credential_writes() {
        let (core, _rx) = tests_harness("lock-blocks", crate::state::fake_dialer());
        set_lock_pin_impl(&core, Zeroizing::new("2468".into())).unwrap();
        let id = upsert_host_impl(&core, draft("db-lock")).unwrap();
        lock_impl(&core).unwrap();
        assert!(lock_status_impl(&core).locked);

        let err = crate::commands::session::open_remote_impl(&core, id.clone(), dbhx_model::PtySize::default())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Locked, "锁定期间不得新拨号");

        let err = set_password_impl(&core, id.clone(), Zeroizing::new("x".into())).unwrap_err();
        assert_eq!(err.code, ErrorCode::Locked, "锁定期间不得写凭据");

        unlock_impl(&core, Zeroizing::new("2468".into())).unwrap();
        assert!(!lock_status_impl(&core).locked);
    }

    #[tokio::test]
    async fn a_wrong_pin_does_not_unlock() {
        let (core, _rx) = tests_harness("lock-wrong", crate::state::fake_dialer());
        set_lock_pin_impl(&core, Zeroizing::new("1234".into())).unwrap();
        lock_impl(&core).unwrap();
        let err = unlock_impl(&core, Zeroizing::new("0000".into())).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert!(lock_status_impl(&core).locked, "错 PIN 绝不能把锁打开");
    }

    #[tokio::test]
    async fn the_pin_never_crosses_ipc() {
        let (core, _rx) = tests_harness("lock-redact", crate::state::fake_dialer());
        set_lock_pin_impl(&core, Zeroizing::new("9876".into())).unwrap();
        let status = lock_status_impl(&core);
        assert!(status.has_pin);
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("9876"), "锁状态绝不带出 PIN: {json}");
        let hosts = hosts_impl(&core).unwrap();
        let json = serde_json::to_string(&hosts).unwrap();
        assert!(!json.contains("9876"), "主机投影绝不带出 PIN: {json}");
    }

    #[tokio::test]
    async fn rotating_the_pin_keeps_one_secret_and_invalidates_the_old_one() {
        let (core, _rx) = tests_harness("lock-rotate", crate::state::fake_dialer());
        set_lock_pin_impl(&core, Zeroizing::new("aaaa".into())).unwrap();
        set_lock_pin_impl(&core, Zeroizing::new("bbbb".into())).unwrap();
        assert_eq!(core.vault.lock().unwrap().secret_count(), 1, "换 PIN 不该留下两把密文");
        lock_impl(&core).unwrap();
        assert_eq!(unlock_impl(&core, Zeroizing::new("aaaa".into())).unwrap_err().code, ErrorCode::PermissionDenied);
        unlock_impl(&core, Zeroizing::new("bbbb".into())).unwrap();
    }

    #[tokio::test]
    async fn clearing_the_pin_unlocks_and_reclaims_the_secret() {
        let (core, _rx) = tests_harness("lock-clear", crate::state::fake_dialer());
        set_lock_pin_impl(&core, Zeroizing::new("4321".into())).unwrap();
        lock_impl(&core).unwrap();
        // 锁着不能改锁，否则等于绕过它——先解锁再清除
        unlock_impl(&core, Zeroizing::new("4321".into())).unwrap();
        clear_lock_pin_impl(&core).unwrap();
        let status = lock_status_impl(&core);
        assert!(!status.has_pin);
        assert!(!status.locked, "没有 PIN 就不该停在锁定态");
        assert_eq!(core.vault.lock().unwrap().secret_count(), 0, "回收 PIN 条目后不该留密文");
    }

    #[tokio::test]
    async fn a_too_short_pin_is_rejected_before_it_is_sealed() {
        let (core, _rx) = tests_harness("lock-short", crate::state::fake_dialer());
        let err = set_lock_pin_impl(&core, Zeroizing::new("12".into())).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(!lock_status_impl(&core).has_pin);
    }

    #[tokio::test]
    async fn setting_a_pin_while_locked_is_refused() {
        // 锁着改锁等于绕过锁：必须先把锁打开
        let (core, _rx) = tests_harness("lock-setpin-locked", crate::state::fake_dialer());
        set_lock_pin_impl(&core, Zeroizing::new("1111".into())).unwrap();
        lock_impl(&core).unwrap();
        let err = set_lock_pin_impl(&core, Zeroizing::new("2222".into())).unwrap_err();
        assert_eq!(err.code, ErrorCode::Locked);
    }

    /// 脱敏总审计：把每一条「朝外」的 IPC 形状都序列化一遍，确认既不含明文秘密，
    /// 也不泄露任何密文引用字段名；再直接读磁盘上的 vault.json，确认落盘的是密文。
    ///
    /// 这条测试是安全约束「明文口令/私钥永不跨 IPC 朝渲染层」与「密文 id 不外泄」的
    /// 回归防线——新增字段很容易不知不觉把一个 `secret_id` 或口令带出去。
    #[tokio::test]
    async fn nothing_crossing_ipc_or_disk_carries_a_secret() {
        let (core, _rx) = tests_harness("redact-audit", crate::state::fake_dialer());

        let host_id = upsert_host_impl(&core, draft("db-redact")).unwrap();
        set_password_impl(&core, host_id.clone(), Zeroizing::new("hunter2".into())).unwrap();
        add_identity_impl(
            &core,
            "运维键".into(),
            Zeroizing::new(crate::test_keys::PLAIN_ED25519.to_string()),
            Some(Zeroizing::new("key-pass".into())),
        )
        .unwrap();
        set_lock_pin_impl(&core, Zeroizing::new("9-8-7-6".into())).unwrap();
        crate::commands::session::open_local_impl(
            &core,
            crate::commands::session::default_local_spec(None, dbhx_model::PtySize::default()),
            "本地".into(),
        )
        .await
        .unwrap();

        // 每一类朝外的投影都拼进同一段文本里统一扫描
        let mut outward = String::new();
        outward.push_str(&serde_json::to_string(&hosts_impl(&core).unwrap()).unwrap());
        outward.push_str(&serde_json::to_string(&identities_impl(&core).unwrap()).unwrap());
        outward.push_str(&serde_json::to_string(&known_hosts_impl(&core).unwrap()).unwrap());
        outward.push_str(&serde_json::to_string(&lock_status_impl(&core)).unwrap());
        outward.push_str(&serde_json::to_string(&crate::commands::session::list_impl(&core)).unwrap());
        outward.push_str(&serde_json::to_string(&crate::commands::transfer::running_impl(&core)).unwrap());

        // 明文秘密：一个都不许出现在 IPC 载荷里
        for leak in ["hunter2", "key-pass", "9-8-7-6", "PRIVATE KEY", "BEGIN OPENSSH"] {
            assert!(!outward.contains(leak), "IPC 载荷泄露了明文 {leak}");
        }
        // 内部引用字段名：渲染层拿到就能指名换出别的记录的口令
        for field in ["cipher", "secret_id", "password_secret_id", "private_key_secret_id", "lock_pin", "secrets"] {
            assert!(!outward.contains(field), "IPC 载荷泄露了内部字段 {field}");
        }

        // 落盘的是密文：vault.json 里绝不该有裸明文
        let on_disk = std::fs::read_to_string(core.dir().join("vault.json")).unwrap();
        for leak in ["hunter2", "key-pass", "9-8-7-6", "PRIVATE KEY"] {
            assert!(!on_disk.contains(leak), "vault.json 明文落盘了 {leak}");
        }

        // 但主密钥文件确实在（否则上面根本加不了密），且不参与任何 IPC
        assert!(core.dir().join("master.key").exists());
    }
}
