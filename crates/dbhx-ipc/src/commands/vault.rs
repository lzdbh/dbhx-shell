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
}
