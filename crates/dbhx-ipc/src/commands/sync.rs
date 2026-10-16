//! 云同步命令：配置的读写，以及「立即同步」那一下。
//!
//! 这一层只加了三件事，都不是白加的：
//!
//! 1. **同步串与令牌只进不出**。对外一律以 `has_sync_str` / `has_token` 两个布尔出现，
//!    值只写进 vault 的密文条目。入方向当场做归一化和长度校验，把「粘错了」在录入
//!    这一次就报出来 —— 之后再发现，症状是「密钥错误」，跟真的密钥错误长得一模一样。
//! 2. **同步不能把 vault 的锁挂在 await 上**。`sync_once` 要一路持有 vault：联网、
//!    两次 Argon2id 派生（实测各约 5s）、再落盘。而 `std::sync::MutexGuard` 不是 `Send`，
//!    异步命令带着它跨 await 直接编译不过。所以整场同步交给一条自己 spawn 的线程，
//!    那里从头到尾是同一个线程，锁和 future 不打架，结果用 channel 递回来。
//! 3. **换片段坐标必须连基线一起作废**。这条规矩本体在 `dbhx-vault::store` 里，
//!    命令层负责不给它开旁路 —— 所以这里没有「只改 id」的写法，一律走 `set_sync_target`。
//!
//! 代价说在前面：同步那十几秒里，其它碰 vault 的命令会排队等锁。合并必须看到一致的
//! 本机状态，半途被人改一条记录，这次合并的结论就不可复现。手动按钮等得起，丢数据等不起。

use std::sync::Arc;

use dbhx_model::{ErrorCode, IpcError, ProviderKind, SyncTarget};
use dbhx_sync::{sync_once, HttpTransport, SyncReport, Transport};
use dbhx_vault::envelope;
use serde::{Deserialize, Serialize};
use specta::Type;
use zeroize::Zeroizing;

use crate::events::{BusEvent, VaultChanged};
use crate::state::Core;

/// 同步配置的可外显形状。两个秘密字段只有「配过没有」，值永不出来。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SyncStatus {
    /// 用户选的那一家。还没有片段坐标时，它决定第一次推送把片段建到哪去。
    pub provider: ProviderKind,
    pub has_sync_str: bool,
    pub has_token: bool,
    /// 云端片段的 id。`None` = 这台设备还没跟着任何一个片段。
    /// 它不是秘密（见 `dbhx_model::SyncTarget`），而且必须显示出来：
    /// 第二台设备光有同步串是找不到云上那一份的。
    pub fragment_id: Option<String>,
    pub include_private_keys: bool,
    /// 本机主机条数。面板用「现在共 N 台」自证同步确实落到本库了。
    pub hosts: usize,
}

/// 这台设备实际会同步到哪一家。取法必须与 `sync_once` 里逐字一致，
/// 否则会出现「面板显示一家、请求打到另一家」。
fn provider_of(vault: &dbhx_vault::Vault) -> ProviderKind {
    vault
        .sync_provider()
        .or_else(|| vault.sync_target().map(|t| t.provider))
        .unwrap_or(ProviderKind::GitHub)
}

pub fn status_impl(core: &Core) -> SyncStatus {
    let vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    SyncStatus {
        provider: provider_of(&vault),
        has_sync_str: vault.has_sync_str(),
        has_token: vault.has_provider_token(),
        fragment_id: vault.sync_target().map(|t| t.id.clone()),
        include_private_keys: vault.sync_private_keys(),
        hosts: vault.hosts().len(),
    }
}

/// 存或忘同步串。`None` 是「忘记本机存的这一份」，云端的数据不动。
pub fn set_str_impl(core: &Core, text: Option<Zeroizing<String>>) -> Result<(), IpcError> {
    core.require_unlocked()?;
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    match text {
        None => vault.clear_sync_str(),
        Some(raw) => {
            let normalized = envelope::normalize(&raw);
            if !envelope::is_usable(&normalized) {
                return Err(IpcError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "同步串至少 {} 个字符，程序生成的那串是 32 个",
                        envelope::MIN_SYNC_STR_LEN
                    ),
                    false,
                ));
            }
            vault.set_sync_str(normalized.as_bytes())?;
        }
    }
    vault.flush()?;
    Ok(())
}

/// 存或忘云端令牌。和同步串分开存的理由见 `VaultDoc::provider_token`：
/// 换令牌不该把同步串也弄丢，反过来也不该逼用户重贴那 32 个字符。
pub fn set_token_impl(core: &Core, text: Option<Zeroizing<String>>) -> Result<(), IpcError> {
    core.require_unlocked()?;
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    match text {
        None => vault.clear_provider_token(),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(IpcError::new(ErrorCode::InvalidInput, "令牌是空的", false));
            }
            vault.set_provider_token(trimmed.as_bytes())?;
        }
    }
    vault.flush()?;
    Ok(())
}

/// 选一家。换家时 vault 会把别家的坐标连同基线一起清掉，这里不必再补一刀。
///
/// 不吃 `require_unlocked`：它既不读出凭据也不改动凭据，只是换一个目的地。
pub fn set_provider_impl(core: &Core, provider: ProviderKind) -> Result<(), IpcError> {
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    vault.set_sync_provider(provider);
    vault.flush()?;
    Ok(())
}

/// 抄进云端片段的 id（第二台设备用）。`None` 或空白 = 不再跟着任何片段。
pub fn set_fragment_impl(core: &Core, id: Option<String>) -> Result<(), IpcError> {
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    let next = match id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(id) if !dbhx_sync::is_valid_fragment_id(id) => {
            return Err(IpcError::new(
                ErrorCode::InvalidInput,
                "片段 id 只要地址里那一串字母数字，网址、空格和斜杠都别带进来",
                false,
            ));
        }
        Some(id) => Some(SyncTarget { provider: provider_of(&vault), id: id.to_string() }),
    };
    // 坐标一变，基线由 vault 一起作废；这里不留「只改 id」的旁路
    vault.set_sync_target(next);
    vault.flush()?;
    Ok(())
}

pub fn set_private_keys_impl(core: &Core, enabled: bool) -> Result<(), IpcError> {
    let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    vault.set_sync_private_keys(enabled);
    vault.flush()?;
    Ok(())
}

/// 走完一次同步。跑完再读一次 `status_impl` 就能拿到新建的片段 id。
pub async fn sync_now_impl(core: Arc<Core>) -> Result<SyncReport, IpcError> {
    core.require_unlocked()?;
    run_sync(core, HttpTransport::new()).await
}

/// 弃掉本机记着的那个片段，用当前这份本机数据在云上重新建一个。
///
/// 用在两种场合：片段在网页上被删了（`recreate_required` 之后），或者用户想从头来一份。
/// 后者要想清楚 —— 另一台设备仍然跟着老片段，直到有人把新 id 抄过去。
/// 这里不提供「删掉云上那一份」：`ProviderError::Missing` 之后云上本来就没东西了，
/// 而真删是另一次授权。
pub async fn sync_recreate_impl(core: Arc<Core>) -> Result<SyncReport, IpcError> {
    core.require_unlocked()?;
    {
        let mut vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
        vault.set_sync_target(None);
        vault.flush()?;
    }
    run_sync(core, HttpTransport::new()).await
}

/// 把一次同步搬到自己的线程上跑完。生产路径走 `HttpTransport`，测试注入假云端 ——
/// 两边跑的是同一个函数，不存在「测的是一条路、上线是另一条」。
async fn run_sync(core: Arc<Core>, transport: impl Transport + Send + 'static) -> Result<SyncReport, IpcError> {
    let vault = Arc::clone(&core.vault);
    let (tx, rx) = std::sync::mpsc::channel();
    // 必须是 std 线程而不是 `spawn_blocking`：tokio 的阻塞线程仍然带着运行时上下文，
    // 在那里新建运行时并 `block_on` 会撞上「Cannot start a runtime from within a runtime」。
    let started = std::thread::Builder::new()
        .name("dbhx-sync".into())
        .spawn(move || {
            // 错误在这一层就翻成 `IpcError`：`SyncError` 到 `IpcError` 的换算本来就在
            // `dbhx-sync` 里定义好，而运行时建不起来是这一层独有的失败，只在这儿判。
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| IpcError::new(ErrorCode::Internal, format!("同步用不起来异步运行时：{e}"), true))
                .and_then(|rt| {
                    rt.block_on(async {
                        let mut guard = vault.lock().unwrap_or_else(|p| p.into_inner());
                        let include_private_keys = guard.sync_private_keys();
                        sync_once(&mut guard, &transport, include_private_keys).await
                    })
                    .map_err(Into::into)
                });
            // 收不到只可能是对面已经不听了，不必处理
            let _ = tx.send(result);
        });
    if started.is_err() {
        return Err(IpcError::new(ErrorCode::Internal, "起不了同步线程，请稍后再试", true));
    }

    // 等结果这一步交给阻塞线程池：worker 线程不该在这儿干等十几秒
    let report = tokio::task::spawn_blocking(move || rx.recv())
        .await
        .map_err(|e| IpcError::new(ErrorCode::Internal, format!("同步任务被丢弃：{e}"), true))?
        .map_err(|_| IpcError::new(ErrorCode::Internal, "同步线程没能把结果送回来", true))?;

    // 主机条数与密文条数都会因合并而变，侧栏的徽章要跟着刷新。
    // 失败也照发：网络断开那一趟可能已经合并了一半，让界面读一次当前真实条数，
    // 比让它继续显示同步前的旧数字稳。
    let vault = core.vault.lock().unwrap_or_else(|p| p.into_inner());
    core.emit(BusEvent::VaultChanged(VaultChanged {
        hosts: vault.hosts().len() as u32,
        secrets: vault.secret_count() as u32,
    }));
    report
}

/// 生成一串新同步串。**只生成、不存**：填进输入框之后仍要用户点保存，
/// 免得「顺手生成了」变成「已经换了一把锁」—— 换串会让云上那一份立刻读不出来。
pub fn new_str_impl() -> String {
    envelope::generate_sync_str()
}

type State<'a> = tauri::State<'a, Arc<Core>>;

#[tauri::command]
#[specta::specta]
pub fn sync_status(core: State<'_>) -> SyncStatus {
    status_impl(&core)
}

/// 存同步串；传 `null` 表示忘记本机存的那一份。
#[tauri::command]
#[specta::specta]
pub fn sync_set_str(core: State<'_>, text: Option<String>) -> Result<(), IpcError> {
    set_str_impl(&core, text.map(Zeroizing::new))
}

#[tauri::command]
#[specta::specta]
pub fn sync_new_str() -> String {
    new_str_impl()
}

/// 存云端令牌；传 `null` 表示忘记。
#[tauri::command]
#[specta::specta]
pub fn sync_set_token(core: State<'_>, text: Option<String>) -> Result<(), IpcError> {
    set_token_impl(&core, text.map(Zeroizing::new))
}

#[tauri::command]
#[specta::specta]
pub fn sync_set_provider(core: State<'_>, provider: ProviderKind) -> Result<(), IpcError> {
    set_provider_impl(&core, provider)
}

/// 抄入云端片段 id；传 `null` 或空串表示不再跟着任何片段。
#[tauri::command]
#[specta::specta]
pub fn sync_set_fragment(core: State<'_>, id: Option<String>) -> Result<(), IpcError> {
    set_fragment_impl(&core, id)
}

#[tauri::command]
#[specta::specta]
pub fn sync_set_private_keys(core: State<'_>, enabled: bool) -> Result<(), IpcError> {
    set_private_keys_impl(&core, enabled)
}

/// 「立即同步」。耗时以十秒计（两次 Argon2id），按钮在此期间必须是 loading 的。
#[tauri::command]
#[specta::specta]
pub async fn sync_now(core: State<'_>) -> Result<SyncReport, IpcError> {
    sync_now_impl(core.inner().clone()).await
}

/// 在云上重建一个片段，用本机这份当起点。
#[tauri::command]
#[specta::specta]
pub async fn sync_recreate(core: State<'_>) -> Result<SyncReport, IpcError> {
    sync_recreate_impl(core.inner().clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::vault::HostDraft;
    use crate::state::tests_harness;
    use dbhx_model::{AuthMode, Endpoint, OsType, Protocol, TerminalEncoding};
    use dbhx_sync::{Method, Response, SyncOutcome, WireError};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const SYNC_STR: &str = "0123456789abcdefghijKLMNOPqrst";
    const TOKEN: &str = "ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE";

    /// 按脚本回应的假云端。和 `dbhx-sync` 里的同名假件一样，**刻意不记录令牌**：
    /// 测试代码多留一份明文副本，只是把「凭据落在更多地方」变成常态。
    #[derive(Clone, Default)]
    struct Fake {
        queued: Arc<Mutex<VecDeque<Result<Response, WireError>>>>,
        sent: Arc<Mutex<Vec<(Method, String)>>>,
    }

    impl Fake {
        fn ok(&self, json: serde_json::Value) {
            self.queued.lock().unwrap().push_back(Ok(Response { status: 200, json }));
        }

        fn fail(&self, err: WireError) {
            self.queued.lock().unwrap().push_back(Err(err));
        }

        fn methods(&self) -> Vec<Method> {
            self.sent.lock().unwrap().iter().map(|(m, _)| *m).collect()
        }
    }

    impl Transport for Fake {
        fn send(&self, req: dbhx_sync::Request) -> dbhx_sync::BoxFuture<Result<Response, WireError>> {
            self.sent.lock().unwrap().push((req.method, req.url.clone()));
            let next = self.queued.lock().unwrap().pop_front();
            Box::pin(async move { next.expect("假云端的脚本用完了") })
        }
    }

    fn configured(core: &Core) {
        set_str_impl(core, Some(Zeroizing::new(SYNC_STR.into()))).unwrap();
        set_token_impl(core, Some(Zeroizing::new(TOKEN.into()))).unwrap();
    }

    fn add_host(core: &Arc<Core>, label: &str) {
        let draft = HostDraft {
            id: None,
            label: label.into(),
            group: None,
            tags: vec![],
            favorite: false,
            protocol: Protocol::Ssh,
            os: OsType::Unknown,
            endpoint: Endpoint { host: "10.0.0.1".into(), port: None, username: "app".into() },
            auth_mode: AuthMode::Password,
            identity_id: None,
            keepalive_secs: 0,
            encoding: TerminalEncoding::Utf8,
        };
        let id = crate::commands::vault::upsert_host_impl(core, draft).unwrap();
        crate::commands::vault::set_password_impl(core, id, Zeroizing::new("pw".into())).unwrap();
    }

    #[tokio::test]
    async fn a_fresh_device_reports_nothing_configured() {
        let (core, _rx) = tests_harness("sync-fresh", crate::state::fake_dialer());
        let status = status_impl(&core);
        assert!(!status.has_sync_str);
        assert!(!status.has_token);
        assert!(status.fragment_id.is_none());
        assert!(!status.include_private_keys, "私钥默认不该跟着走");
        assert_eq!(status.provider, ProviderKind::GitHub);
    }

    /// 状态投影是这条命令面上唯一「自动朝外」的东西，必须只有布尔和一个 id。
    #[tokio::test]
    async fn the_status_never_carries_the_string_or_the_token() {
        let (core, _rx) = tests_harness("sync-redact", crate::state::fake_dialer());
        configured(&core);
        let json = serde_json::to_string(&status_impl(&core)).unwrap();
        assert!(!json.contains(SYNC_STR), "状态里带出了同步串: {json}");
        assert!(!json.contains(TOKEN), "状态里带出了令牌: {json}");
        assert!(json.contains("has_sync_str"), "{json}");
    }

    #[tokio::test]
    async fn a_too_short_string_is_refused_and_nothing_is_stored() {
        let (core, _rx) = tests_harness("sync-short", crate::state::fake_dialer());
        let err = set_str_impl(&core, Some(Zeroizing::new("  太短  ".into()))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(!status_impl(&core).has_sync_str, "校验失败不能留下一条半截配置");
    }

    #[tokio::test]
    async fn whitespace_from_pasting_is_normalized_away() {
        let (core, _rx) = tests_harness("sync-paste", crate::state::fake_dialer());
        // 从网页上复制常多带一个换行；归一化必须在录入这一次做掉
        set_str_impl(&core, Some(Zeroizing::new(format!("{SYNC_STR}\r\n").into()))).unwrap();
        let stored = core
            .vault
            .lock()
            .unwrap()
            .with_sync_str(|p| Ok(String::from_utf8(p.to_vec())))
            .unwrap()
            .unwrap();
        assert_eq!(stored, SYNC_STR);
    }

    #[tokio::test]
    async fn forgetting_the_string_keeps_the_token() {
        let (core, _rx) = tests_harness("sync-forget", crate::state::fake_dialer());
        configured(&core);
        set_str_impl(&core, None).unwrap();
        let status = status_impl(&core);
        assert!(!status.has_sync_str);
        assert!(status.has_token, "忘记同步串不该把令牌也一起清了");
    }

    #[tokio::test]
    async fn a_pasted_url_is_refused_before_it_reaches_the_vault() {
        let (core, _rx) = tests_harness("sync-bad-id", crate::state::fake_dialer());
        for bad in ["https://gist.github.com/abc", "abc def", "../../evil", "abc?token=1"] {
            let err = set_fragment_impl(&core, Some(bad.into())).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidInput, "{bad} 本该被挡在录入这一步");
        }
        assert!(status_impl(&core).fragment_id.is_none());
        set_fragment_impl(&core, Some("3d0rmjto24pfie6bawxsu73".into())).unwrap();
        assert_eq!(status_impl(&core).fragment_id.as_deref(), Some("3d0rmjto24pfie6bawxsu73"));
    }

    /// 换一家时坐标一起没掉，这是 vault 侧的规矩；命令层要保证界面看得见这件事，
    /// 否则用户以为「切回去 id 还在」。
    #[tokio::test]
    async fn switching_provider_moves_the_fragment_with_it() {
        let (core, _rx) = tests_harness("sync-switch", crate::state::fake_dialer());
        set_provider_impl(&core, ProviderKind::Gitee).unwrap();
        set_fragment_impl(&core, Some("3d0rmjto24pfie6bawxsu73".into())).unwrap();
        assert_eq!(status_impl(&core).provider, ProviderKind::Gitee);
        set_provider_impl(&core, ProviderKind::GitHub).unwrap();
        let status = status_impl(&core);
        assert_eq!(status.provider, ProviderKind::GitHub);
        assert!(status.fragment_id.is_none(), "gitee 的 id 不该留在切到 github 的配置里");
    }

    #[tokio::test]
    async fn the_private_keys_switch_persists_through_the_command() {
        let (core, _rx) = tests_harness("sync-keys", crate::state::fake_dialer());
        set_private_keys_impl(&core, true).unwrap();
        assert!(status_impl(&core).include_private_keys);
        let dir = core.dir().to_path_buf();
        drop(core);
        let reopened =
            dbhx_vault::Vault::open(&dir, dbhx_vault::KeySource::File(dir.join("master.key"))).unwrap();
        assert!(reopened.sync_private_keys(), "勾了却没落盘，下次推的仍然是少一块的包");
    }

    /// 没配齐时点「立即同步」：不碰网络，也不改本机。
    #[tokio::test]
    async fn syncing_before_configuring_does_nothing_at_all() {
        let (core, _rx) = tests_harness("sync-unconfigured", crate::state::fake_dialer());
        let fake = Fake::default();
        let report = run_sync(core, fake.clone()).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::NotConfigured);
        assert_eq!(report.hosts, 0);
        assert!(fake.methods().is_empty(), "没配齐就不该有请求");
    }

    /// 第一次同步且本地已经有内容：建片段、把 id 记进 vault、发一次变更事件。
    ///
    /// 这条走的是真的 `sync_once`，付一次 Argon2id（实测约 5s）—— 值得，因为它证明的
    /// 是「命令层把 vault、provider、事件三条线接对了」，而不是各自单测自己那一块。
    #[tokio::test]
    async fn the_first_sync_creates_the_fragment_and_reports_its_id() {
        let (core, mut rx) = tests_harness("sync-create", crate::state::fake_dialer());
        configured(&core);
        add_host(&core, "本机的一台");

        let fake = Fake::default();
        fake.ok(serde_json::json!({ "id": "newlymadeid" }));
        let report = run_sync(Arc::clone(&core), fake.clone()).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::Created);
        assert_eq!(report.hosts, 1);
        assert_eq!(fake.methods(), vec![Method::Post], "还没有坐标时不该先 GET");

        let status = status_impl(&core);
        assert_eq!(status.fragment_id.as_deref(), Some("newlymadeid"), "新建的 id 必须让面板看得见");
        let mut got_change = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, BusEvent::VaultChanged(_)) {
                got_change = true;
            }
        }
        assert!(got_change, "同步改动了本机库却没有发变更事件");
    }

    /// 云上那个片段被人在网页上删了：这一趟必须原样回来，等用户点头重建。
    #[tokio::test]
    async fn a_missing_fragment_asks_before_recreating() {
        let (core, _rx) = tests_harness("sync-gone", crate::state::fake_dialer());
        configured(&core);
        add_host(&core, "本机的一台");
        set_fragment_impl(&core, Some("deadbeef".into())).unwrap();
        let fake = Fake::default();
        fake.fail(WireError::NotFound);
        let report = run_sync(Arc::clone(&core), fake.clone()).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::RecreateRequired);
        assert_eq!(
            status_impl(&core).fragment_id.as_deref(),
            Some("deadbeef"),
            "没经用户点头不许把坐标抹掉"
        );
    }

    /// 令牌被云端拒掉必须回 `auth_failed`，不能和「本机解不开云上那份」挤进
    /// 同一个码：前端就靠这两个码决定是让用户去换令牌，还是去重贴同步串。
    #[tokio::test]
    async fn a_rejected_token_comes_back_as_auth_failed_not_a_bad_sync_string() {
        let (core, _rx) = tests_harness("sync-bad-token", crate::state::fake_dialer());
        configured(&core);
        add_host(&core, "本机的一台");
        set_fragment_impl(&core, Some("deadbeef".into())).unwrap();
        let fake = Fake::default();
        fake.fail(WireError::Unauthorized);
        let err = run_sync(Arc::clone(&core), fake).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::AuthFailed);
        assert!(!err.retryable, "令牌错重试一百次还是错");
        assert!(err.message.contains("令牌"), "{:?}", err.message);
        assert!(!err.message.contains(SYNC_STR), "错误文案带出了同步串: {:?}", err.message);
    }

    /// 「串和令牌都配好了，可本机一条主机也没有」—— 第一次点不该在云上占一个片段。
    #[tokio::test]
    async fn an_empty_library_is_not_published_as_a_fragment() {
        let (core, _rx) = tests_harness("sync-empty", crate::state::fake_dialer());
        configured(&core);
        let fake = Fake::default();
        let report = run_sync(Arc::clone(&core), fake.clone()).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::Nothing);
        assert!(fake.methods().is_empty(), "空库不该往云上建东西: {:?}", fake.methods());
    }

    #[test]
    fn the_generated_string_is_the_one_the_envelope_accepts() {
        // 生成器与校验器必须互相认得：一边生成 32 位、一边要求别的形状的话，
        // 「生成→直接保存」这条路会在命令层被自己挡死
        let s = new_str_impl();
        assert_eq!(s.chars().count(), 32);
        assert!(envelope::is_usable(&envelope::normalize(&s)));
    }
}
