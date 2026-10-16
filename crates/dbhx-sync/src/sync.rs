//! 一次「立即同步」的完整编排：读云端 → 解开 → 合并 → 落地 → （只在确实有变化时）推回去。
//!
//! 这一层是 vault 与 provider 之间唯一的粘合处，两边都不许越过它互相认识：
//! `dbhx-vault` 只管「给我一把密钥和一份清单，我把本地改成那样」，
//! `dbhx-sync::provider` 只管「给我一份已经封好的字节，我放上云上」。
//! 于是合并结论对不对、云端形状对不对，各自都能在自己的测试里钉住，
//! 而这里只钉一件事：**顺序，以及这次到底该不该联网、该不该写字**。
//!
//! 两条花钱的规矩是这一层存在的理由：
//! - **派生最多两次，且各只在真正需要时才付**。Argon2id 单次实测约 5s，用户点完
//!   按钮要等的就是这段时间。拉取时解云上那一份一次，推送时封新的一份一次；
//!   「云上和本地一样」那次同步应该只花一次派生就返回。
//! - **推上去的盐每次都是新的**。gist 的历史 revision 删不掉，云端因此同时躺着
//!   好几份密文；复用盐等于给这堆历史密文配同一把钥匙，攻破一份就攻破全部。

use dbhx_model::{ProviderKind, SyncTarget};
use dbhx_vault::envelope;
use dbhx_vault::snapshot::{MergeInput, MergePlan, MergePolicy, SyncSnapshot};
use dbhx_vault::store::new_id;
use dbhx_vault::{DerivedKey, Vault, VaultError};
use zeroize::Zeroizing;

use crate::provider::{ProviderError, SyncClient};
use crate::wire::Transport;

/// 一次同步跑到了哪一步。设置面板拿它决定念哪句话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    /// 同步串或云端令牌还没配齐。**没碰网络，也没改本地。**
    NotConfigured,
    /// 本地记着的片段在云上已经不存在了。
    ///
    /// 这里刻意不擅自新建一份：同一个账号下很可能还躺着另一台设备刚推上去的、
    /// 比本地更新的数据，悄悄建第二个片段会让两台设备从此各写各的 ——
    /// 而那正是这套设计最想避免的事故。得由用户点头重建。
    RecreateRequired,
    /// 两边内容一致，什么都没动。
    Nothing,
    /// 从云上取回了东西，本地已合并落盘；云上无需再写。
    ///
    /// 「新设备第一次同步」正常落在这一档：它只该读，不该白写一遍。
    Pulled,
    /// 第一次同步：云上本来没有片段，这是刚建出来的那一份。
    Created,
    /// 本地（含刚合并进来的远端改动）比云上那一份新，已经回推成功。
    Pushed,
}

/// 一次同步的结果。只带名字和条数，不带任何凭据内容。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncReport {
    pub outcome: SyncOutcome,
    /// 合并完本机有几台主机。UI 用「现在共 N 台」一句自证同步确实生效了。
    pub hosts: usize,
    /// 这次写入本机的主机条数（含转存的冲突副本）。
    pub imported: usize,
    /// 两边都改过同一条而转存成「（冲突副本）」的条目名。
    pub conflicts: Vec<String>,
    /// 认证方式要私钥、但本机仍没有对应身份的主机名。
    pub missing_identities: Vec<String>,
    /// 口令既不在本地也不在云上、需要重设一次口令的主机名。
    pub missing_credentials: Vec<String>,
}

impl SyncReport {
    fn of(outcome: SyncOutcome, vault: &Vault, plan: &MergePlan) -> Self {
        Self {
            outcome,
            hosts: vault.hosts().len(),
            imported: plan.hosts_upsert.len(),
            conflicts: plan.conflicts.clone(),
            missing_identities: plan.missing_identities.clone(),
            missing_credentials: plan.missing_credentials.clone(),
        }
    }
}

/// 编排层能遇到的失败。
///
/// 「没配好」与「云端片段被删」不在这里：那是需要用户处置的**状态**，
/// 不是这次操作的失败，走 `SyncOutcome` 才贴合 UI 要做的事。
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Cloud(#[from] ProviderError),
    #[error(transparent)]
    Vault(#[from] VaultError),
}

impl From<SyncError> for dbhx_model::IpcError {
    fn from(e: SyncError) -> Self {
        // 两家的错误各自已经知道该映射成哪个 `ErrorCode`，这里不许自作聪明改判。
        match e {
            SyncError::Cloud(p) => p.into(),
            SyncError::Vault(v) => v.into(),
        }
    }
}

/// 走完一次同步。`include_private_keys` 就是设置里那个默认不勾的开关。
///
/// 无论中途成败，**已经合并进本地的远端改动都会落盘**：那是用户的数据，
/// 不能因为「推回去时网断了」就丢。反过来，推送失败时基线一步都不动，
/// 于是下一次点「立即同步」仍然会把这次的成果推上去。
///
/// 新设备要在这里读出云上那一份，得先有片段坐标：id 是云端随机生成的，
/// 从同步串推不出来，所以第二台设备必须由人把「同步串 + 片段 id」两样都填进来
/// （见 `SyncClient::fetch`：没有 id 就不碰网络，直接当作「云上还没有」）。
pub async fn sync_once(
    vault: &mut Vault,
    transport: &dyn Transport,
    include_private_keys: bool,
) -> Result<SyncReport, SyncError> {
    if !vault.has_sync_str() || !vault.has_provider_token() {
        return Ok(SyncReport::of(SyncOutcome::NotConfigured, vault, &MergePlan::default()));
    }
    // 这两行是明文凭据在本进程里唯一的露面处，出函数作用域就没了：
    // 同步串拿去派生（`normalize` 幂等，为的是容下老代码存进去的带空白串），
    // 令牌交给 `SyncClient` 攥着 —— 它自己也进不了 `Debug`。
    let sync_str = vault.with_sync_str(|p| Ok(envelope::normalize(&String::from_utf8_lossy(p))))?;
    let token =
        vault.with_provider_token(|p| Ok(Zeroizing::new(String::from_utf8_lossy(p).into_owned())))?;
    let provider = vault
        .sync_target()
        .map(|t| t.provider)
        .unwrap_or(ProviderKind::GitHub);
    let had_target = vault.sync_target().is_some();
    let client = SyncClient::new(provider, vault.sync_target().map(|t| t.id.clone()), token);

    // 基线读不出来就按「没有基线」继续：合并结论会变保守（宁可转存冲突副本），
    // 但绝不能把它当成「本来就没有」，那等于允许新设备直接盖掉云上那一份。
    let base = match vault.sync_base() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "同步基线读不出来，这次按没有基线保守合并");
            None
        }
    };

    let cloud = match client.fetch(transport).await {
        Ok(c) => c,
        Err(ProviderError::Missing) => {
            return Ok(SyncReport::of(SyncOutcome::RecreateRequired, vault, &MergePlan::default()))
        }
        Err(e) => return Err(e.into()),
    };

    let mut plan = MergePlan::default();
    let mut remote: Option<SyncSnapshot> = None;
    if let Some(blob) = &cloud {
        // 派生 #1。这把密钥解得了云上那一份、封不了新东西（盐属于那条历史 revision），
        // 类型系统里就写着这件事，所以不存在「顺手拿它推回去」的写法。
        let (pull_key, snap) = DerivedKey::from_blob(&sync_str, &blob.bytes)?;
        plan = three_way(vault, &snap, base.as_ref());
        vault.apply_plan(&pull_key, &plan)?;
        remote = Some(snap);
    }

    // 「要不要推」只跟一件事比：本地现在这份，跟上一次落在云上那份是不是同一个内容。
    // 比的是记录与密文 id，不比密文字节 —— 每次重密封的 nonce 都是随机的，比字节恒不等。
    let local = vault.local_snapshot(include_private_keys)?;
    let unchanged = match (&remote, &base) {
        // 刚把云上那一份合并进来：本地与它内容一致，就说明本机没有自己的新东西要传。
        // 比基线更省一次写 —— 凡是「拉到了新东西」的同步，基线必然与本地不同，
        // 拿基线来判断会把新设备的首次只读同步也变成一次多余的推送。
        (Some(r), _) => r.same_content(&local),
        // 云上是空的（本机第一次同步，或那个片段刚被删干净）：只跟基线比。
        (None, Some(b)) => b.same_content(&local),
        (None, None) => !is_worth_publishing(&local),
    };

    let mut pushed = false;
    let mut push_error: Option<SyncError> = None;
    if !unchanged {
        match push(vault, &client, transport, &sync_str, include_private_keys).await {
            Ok(target) => {
                vault.set_sync_target(Some(target));
                pushed = true;
            }
            Err(e) => push_error = Some(e),
        }
    }

    // 基线的含义是「云上现在就是这一份」。推成功、或者根本没推但两边本来就一样，
    // 都配得上这句话 —— 每次同步都要把它刷新，否则下一次合并会拿一份过期的底子
    // 定胜负，把「对方只是取回过」误判成「对方也改过」而多转存冲突副本。
    // 云上根本没有片段（第一次同步且本地为空）时不记：那时这句话没有主语。
    if pushed || (unchanged && remote.is_some()) {
        vault.set_sync_base(Some(&local))?;
    }
    // 落盘放在报错之前：合并结果与新建的片段坐标都是用户已经拿到的东西。
    vault.flush()?;
    if let Some(e) = push_error {
        return Err(e);
    }

    let outcome = if pushed {
        if had_target {
            SyncOutcome::Pushed
        } else {
            SyncOutcome::Created
        }
    } else if plan.is_empty() {
        SyncOutcome::Nothing
    } else {
        SyncOutcome::Pulled
    };
    Ok(SyncReport::of(outcome, vault, &plan))
}

/// 这份本地快照里有没有值得占一个云端片段的东西。
fn is_worth_publishing(snap: &SyncSnapshot) -> bool {
    !(snap.hosts.is_empty() && snap.identities.is_empty() && snap.tombstones.is_empty())
}

/// 用三方合并算出这次要把本地改成什么样。
///
/// 口径写死 `ThreeWay`：手动按钮给用户的预期就是「两边都保留、有分歧就另存副本」，
/// 而 `LocalWins` / `RemoteWins` 那种整库口径必须由 UI 明确勾选才敢用。
fn three_way(vault: &Vault, remote: &SyncSnapshot, base: Option<&SyncSnapshot>) -> MergePlan {
    let ids = vault.secret_ids();
    let input = MergeInput {
        local_hosts: vault.hosts(),
        local_identities: vault.identities(),
        local_tombstones: vault.tombstones(),
        local_device_id: vault.device_id(),
        local_secret_ids: &ids,
        remote,
        base,
        policy: MergePolicy::ThreeWay,
    };
    dbhx_vault::snapshot::plan_merge(&input, new_id)
}

/// 封一份新的推上去。这里付第二次派生，也**只在真要写的时候**才走到这一行。
///
/// 重新走一遍 `export_snapshot` 而不是复用上面那份 `local_snapshot`：两者的记录与
/// 密文 id 完全相同（同一段收集逻辑），但快照里每条密文要换的锁不一样 ——
/// 云上那份必须用同步密钥封，本机的基线用主密钥封。
async fn push(
    vault: &Vault,
    client: &SyncClient,
    transport: &dyn Transport,
    sync_str: &str,
    include_private_keys: bool,
) -> Result<SyncTarget, SyncError> {
    let push_key = DerivedKey::for_push(sync_str)?;
    let snap = vault.export_snapshot(&push_key, include_private_keys)?;
    let blob = push_key.seal_snapshot(&snap)?;
    Ok(client.push(transport, &blob).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::SYNC_FILE_NAME;
    use crate::wire::{Method, Response, WireError};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use dbhx_model::{AuthMode, Endpoint, HostId, HostRecord, OsType, Protocol, TerminalEncoding};
    use dbhx_vault::{KeySource, MasterKey};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, OnceLock};

    const SYNC_STR: &str = "0123456789abcdefghijKLMNOPqrst";
    const FAKE_TOKEN: &[u8] = b"ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE";

    /// 按脚本回应的假传输。记下来的只有方法、地址和请求体，**刻意不存令牌** ——
    /// 测试代码自己复制一份令牌，就成了「凭据落在更多地方」的反面样板。
    #[derive(Default)]
    struct Fake {
        queued: Arc<Mutex<VecDeque<Result<Response, WireError>>>>,
        sent: Arc<Mutex<Vec<(Method, String, Option<serde_json::Value>)>>>,
    }

    impl Fake {
        fn new() -> Self {
            Self::default()
        }

        fn ok(&self, json: serde_json::Value) {
            self.queued.lock().unwrap().push_back(Ok(Response { status: 200, json }));
        }

        fn fail(&self, err: WireError) {
            self.queued.lock().unwrap().push_back(Err(err));
        }

        /// 最后一次请求体里那个文件的内容 —— 拿来把「我们真的推了什么」喂回下一次 GET。
        fn last_content(&self) -> String {
            let sent = self.sent.lock().unwrap();
            let body = sent.last().expect("没有发过任何请求").2.clone().expect("请求体为空");
            body["files"][SYNC_FILE_NAME]["content"]
                .as_str()
                .expect("创建/更新的请求体里没有那个文件")
                .to_string()
        }

        fn methods(&self) -> Vec<Method> {
            self.sent.lock().unwrap().iter().map(|(m, _, _)| *m).collect()
        }

        fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    impl Transport for Fake {
        fn send(&self, req: crate::wire::Request) -> crate::wire::BoxFuture<Result<Response, WireError>> {
            self.sent
                .lock()
                .unwrap()
                .push((req.method, req.url.clone(), req.body.clone()));
            let next = self.queued.lock().unwrap().pop_front();
            Box::pin(async move {
                match next {
                    Some(r) => r,
                    // 取完还来请求 = 多写了一次云上。这条 panic 比任何断言都直白。
                    None => panic!("脚本里没有更多响应了，说明发多了请求"),
                }
            })
        }
    }

    fn host(label: &str) -> HostRecord {
        HostRecord {
            id: HostId(String::new()),
            label: label.into(),
            group: None,
            tags: vec![],
            favorite: false,
            protocol: Protocol::Ssh,
            os: OsType::Linux,
            endpoint: Endpoint {
                host: "10.0.0.1".into(),
                port: Some(22),
                username: "app".into(),
            },
            auth_mode: AuthMode::Password,
            identity_id: None,
            password_secret_id: None,
            keepalive_secs: 0,
            encoding: TerminalEncoding::Utf8,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("dbhx-sync-{tag}-{}-{}", std::process::id(), new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 一台配好同步串与令牌、且至少有一条主机的库。
    /// 令牌是随手编的假串：这些用例一律走假传输，碰不到任何真账号。
    fn configured_vault(dir: &std::path::Path) -> Vault {
        let mut v = Vault::open(dir, KeySource::Memory(MasterKey::generate())).unwrap();
        v.set_sync_str(SYNC_STR.as_bytes()).unwrap();
        v.set_provider_token(FAKE_TOKEN).unwrap();
        v
    }

    fn vault_with_host(dir: &std::path::Path, label: &str) -> Vault {
        let mut v = configured_vault(dir);
        let hid = v.upsert_host(host(label));
        v.set_host_password(&hid, b"pw").unwrap();
        v
    }

    fn gist_json(content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "abc123",
            "updated_at": "2026-10-16T00:00:00Z",
            "files": { SYNC_FILE_NAME: { "content": content, "truncated": false } }
        })
    }

    /// 另一台设备（B）推上去的那一份。只派生一次，好几个用例共用。
    ///
    /// 必须由**同步密钥**封：`apply_plan` 解里面的密文条目用的就是那把钥匙，
    /// 用别的锁造出来的假 blob 只会测到「解不开」那一条分支。
    fn other_devices_blob() -> &'static [u8] {
        static BLOB: OnceLock<Vec<u8>> = OnceLock::new();
        BLOB.get_or_init(|| {
            let key = DerivedKey::for_push(SYNC_STR).unwrap();
            let mut snap = SyncSnapshot::new("device-B".into(), 1_000);
            let mut h = host("B那边建的一台");
            h.id = HostId("h-from-b".into());
            h.updated_at_ms = 900;
            snap.hosts = vec![h];
            key.seal_snapshot(&snap).unwrap()
        })
    }

    #[tokio::test]
    async fn a_missing_token_or_sync_string_stops_before_the_network() {
        let dir = temp_dir("unconfigured");
        let mut v = Vault::open(&dir, KeySource::Memory(MasterKey::generate())).unwrap();
        let hid = v.upsert_host(host("有台主机但没配同步"));
        v.set_host_password(&hid, b"pw").unwrap();
        let fake = Fake::new();
        let report = sync_once(&mut v, &fake, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::NotConfigured);
        assert_eq!(fake.count(), 0, "没配齐就该一个请求都不发");

        // 只补上同步串还不够：缺令牌同样连不上任何一家，而这是更常见的半途状态。
        v.set_sync_str(SYNC_STR.as_bytes()).unwrap();
        let report = sync_once(&mut v, &fake, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::NotConfigured);
        assert_eq!(fake.count(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn first_sync_on_an_empty_cloud_creates_the_gist_and_keeps_its_id() {
        let dir = temp_dir("first-sync");
        let mut v = vault_with_host(&dir, "本机的一台");
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "deadbeef" }));
        let report = sync_once(&mut v, &fake, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::Created);
        assert_eq!(fake.methods(), vec![Method::Post], "云上还没有，只该创建一次");
        let target = v.sync_target().expect("新建的片段坐标必须记下来");
        assert_eq!(target.id, "deadbeef");
        assert_eq!(target.provider, ProviderKind::GitHub);
        assert!(v.sync_base().unwrap().is_some(), "推成功了就该有基线，否则下次还是整库定胜负");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn an_empty_local_list_does_not_open_a_brand_new_gist() {
        let dir = temp_dir("empty-list");
        let mut v = configured_vault(&dir);
        let fake = Fake::new();
        let report = sync_once(&mut v, &fake, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::Nothing);
        assert_eq!(fake.count(), 0, "空列表不值得在云上开一个片段");
        assert!(v.sync_target().is_none());
        assert!(v.sync_base().unwrap().is_none(), "云上根本没有片段时谈不上「与云端一致」");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 只读的一次同步也要把基线刷新成合并后的本地。
    ///
    /// 停在同步前那份的话，下一次合并会拿一份过期的底子定胜负：本机只是从云上
    /// 取回了一条记录，却被判成「两边都改过同一条」，凭空多出一堆冲突副本。
    #[tokio::test]
    async fn a_pull_without_a_push_still_refreshes_the_baseline() {
        let dir = temp_dir("baseline-refresh");
        let mut v = configured_vault(&dir);
        v.set_sync_target(Some(SyncTarget { provider: ProviderKind::GitHub, id: "abc123".into() }));
        let fake = Fake::new();
        fake.ok(gist_json(&URL_SAFE_NO_PAD.encode(other_devices_blob())));
        sync_once(&mut v, &fake, false).await.unwrap();
        let base = v.sync_base().unwrap().expect("只读这一次也该有基线");
        assert_eq!(base.hosts.len(), 1, "基线该是合并完的本地，不是同步前那份空的");
        assert_eq!(base.device_id, v.device_id(), "基线是本机的快照，不是云端那份的复制");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 连点两次「立即同步」不该把同一份数据再传一遍。
    /// 多那一次不只是多花 5s：它还会在云上留下一条**删不掉**的多余 revision。
    #[tokio::test]
    async fn a_second_sync_with_nothing_changed_reads_only_and_writes_nothing() {
        let dir = temp_dir("idempotent");
        let mut v = vault_with_host(&dir, "本机的一台");

        let first = Fake::new();
        first.ok(serde_json::json!({ "id": "deadbeef" }));
        sync_once(&mut v, &first, false).await.unwrap();
        // 把第一次真正推上去的那份原样当成「云上现在的内容」，不加任何加工。
        let content = first.last_content();

        let second = Fake::new();
        second.ok(gist_json(&content));
        let report = sync_once(&mut v, &second, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::Nothing);
        assert_eq!(second.methods(), vec![Method::Get], "内容没变就不该再写云上");
        assert_eq!(v.sync_target().unwrap().id, "deadbeef", "不能因为又推了一次就换片段");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 新设备第一次同步：云上那台该取回来，而且**不该**顺手白写一遍云上。
    ///
    /// 目标坐标是用户手工填的：片段 id 是云端随机生成的，推不出来，
    /// 所以第二台设备只能由人把「同步串 + 片段 id」两样都抄过去。
    #[tokio::test]
    async fn a_new_device_pulls_the_cloud_list_without_pushing_the_union_back() {
        let dir = temp_dir("new-device");
        let mut v = configured_vault(&dir);
        v.set_sync_target(Some(SyncTarget { provider: ProviderKind::GitHub, id: "abc123".into() }));
        let fake = Fake::new();
        fake.ok(gist_json(&URL_SAFE_NO_PAD.encode(other_devices_blob())));
        let report = sync_once(&mut v, &fake, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::Pulled);
        assert_eq!(
            fake.methods(),
            vec![Method::Get],
            "本地没有任何东西要传，白推一次等于多一条删不掉的历史"
        );
        assert_eq!(v.hosts().len(), 1, "云上那台要落到本机");
        assert_eq!(report.imported, 1);
        assert!(
            v.sync_base().unwrap().is_some(),
            "只读这一次也要立起基线，否则下次仍无从判断谁改过"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_gist_deleted_on_the_web_asks_the_user_to_recreate_and_changes_nothing() {
        let dir = temp_dir("gist-gone");
        let mut v = vault_with_host(&dir, "本机的一台");
        v.set_sync_target(Some(SyncTarget { provider: ProviderKind::GitHub, id: "deadbeef".into() }));
        let fake = Fake::new();
        fake.fail(WireError::NotFound);
        let report = sync_once(&mut v, &fake, false).await.unwrap();
        assert_eq!(report.outcome, SyncOutcome::RecreateRequired);
        assert_eq!(fake.methods(), vec![Method::Get]);
        assert_eq!(v.sync_target().unwrap().id, "deadbeef", "没经用户点头不许把坐标抹掉");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 推送失败是最容易丢数据的一步：拉回来的改动必须已经在盘上，
    /// 而基线必须还没更新，否则下一次点按钮会把「没推上去」当成「已经一致」。
    #[tokio::test]
    async fn a_failed_push_keeps_pulled_records_on_disk_and_leaves_the_baseline_alone() {
        let dir = temp_dir("push-fails");
        let key_path = dir.join("master.key");
        let mut v = Vault::open(&dir, KeySource::File(key_path.clone())).unwrap();
        v.set_sync_str(SYNC_STR.as_bytes()).unwrap();
        v.set_provider_token(FAKE_TOKEN).unwrap();
        let hid = v.upsert_host(host("本机自己建的一台"));
        v.set_host_password(&hid, b"pw").unwrap();
        v.set_sync_target(Some(SyncTarget { provider: ProviderKind::GitHub, id: "abc123".into() }));

        let fake = Fake::new();
        // 云上那份是 B 推的：合并完本地有两台，于是必须回推 —— 而这次回推被拒了。
        fake.ok(gist_json(&URL_SAFE_NO_PAD.encode(other_devices_blob())));
        fake.fail(WireError::Unauthorized);
        let err = sync_once(&mut v, &fake, false).await.unwrap_err();
        assert!(matches!(err, SyncError::Cloud(ProviderError::Wire(WireError::Unauthorized))));
        assert_eq!(fake.methods(), vec![Method::Get, Method::Patch]);

        drop(v);
        let reopened = Vault::open(&dir, KeySource::File(key_path)).unwrap();
        assert_eq!(reopened.hosts().len(), 2, "取回来的那台不能因为推送失败就丢了");
        assert!(reopened.sync_base().unwrap().is_none(), "推失败还更新基线，下次会把该推的当成已一致");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 真联网走完整条链路：A 推 → B 拉 → A 再改 → 两边各自再同步。
    ///
    /// 只有 `HttpTransport` 能证明的那件事，这里的假传输证不了：**B 用的是另一把
    /// 本机主密钥**。口令在 A 上封存、经同步密钥重封上云、在 B 上再转成本机封存，
    /// 全程不落明文 —— 换设备这件事只有这样才叫自证过一次。
    ///
    /// 测完删掉片段；删不掉时把 id 印在报错里，好让人（或我）手动收拾。
    #[tokio::test]
    #[ignore = "需要真实令牌，手动跑"]
    async fn two_devices_really_meet_through_the_github_api() {
        let path = std::env::var("DBHX_TOKEN_FILE").expect("设 DBHX_TOKEN_FILE 指向令牌文件");
        let raw = std::fs::read(&path).expect("读不到令牌文件");
        let token = Zeroizing::new(
            String::from_utf8(raw).expect("令牌文件不是 UTF-8").trim().to_string(),
        );
        assert!(!token.is_empty(), "令牌文件是空的");
        // 现造一串，免得把测试用的同步串写进仓库里
        let sync_str = envelope::generate_sync_str();
        let transport = crate::wire::HttpTransport::new();

        let dir_a = temp_dir("live-a");
        let dir_b = temp_dir("live-b");
        let mut a = Vault::open(&dir_a, KeySource::Memory(MasterKey::generate())).unwrap();
        let mut b = Vault::open(&dir_b, KeySource::Memory(MasterKey::generate())).unwrap();
        for v in [&mut a, &mut b] {
            v.set_sync_str(sync_str.as_bytes()).unwrap();
            v.set_provider_token(token.as_bytes()).unwrap();
        }

        for (label, pw) in [("生产库", "pw-prod".as_bytes()), ("测试库", "pw-test".as_bytes())] {
            let hid = a.upsert_host(host(label));
            a.set_host_password(&hid, pw).unwrap();
        }
        let report = sync_once(&mut a, &transport, false).await.expect("A 首同步失败");
        assert_eq!(report.outcome, SyncOutcome::Created);
        let target = a.sync_target().cloned().expect("没记下新建的片段坐标");
        b.set_sync_target(Some(target.clone()));

        let report = sync_once(&mut b, &transport, false).await.expect("B 拉取失败");
        assert_eq!(report.outcome, SyncOutcome::Pulled, "新设备只该读，不该白写一遍");
        assert_eq!(b.hosts().len(), 2, "云上两台都要落到 B");
        let mut got = Vec::new();
        for h in b.hosts() {
            let sid = h.password_secret_id.clone().expect("口令密文该跟着主机一起过来");
            let plain = b.with_secret(&sid, |p| Ok(p.to_vec())).unwrap();
            got.push((h.label.clone(), String::from_utf8(plain).unwrap()));
        }
        got.sort();
        assert_eq!(
            got,
            vec![("测试库".to_string(), "pw-test".to_string()), ("生产库".to_string(), "pw-prod".to_string())],
            "B 用另一把主密钥转封之后，口令读出来变了"
        );

        // A 再加一台：先拉后推，B 那边跟着取回，再点一次就只剩一次 GET
        let extra = a.upsert_host(host("临时的另一台"));
        a.set_host_password(&extra, b"pw-extra").unwrap();
        let report = sync_once(&mut a, &transport, false).await.expect("A 回推失败");
        assert_eq!(report.outcome, SyncOutcome::Pushed);
        let report = sync_once(&mut b, &transport, false).await.expect("B 二次拉取失败");
        assert_eq!(report.outcome, SyncOutcome::Pulled);
        assert_eq!(b.hosts().len(), 3, "A 后加的那台该跟着过来");
        let report = sync_once(&mut b, &transport, false).await.expect("B 第三次拉取失败");
        assert_eq!(report.outcome, SyncOutcome::Nothing, "两边已经一致，不该再写云上");

        let cleanup = SyncClient::from_target(&target, token.clone());
        cleanup
            .delete(&transport)
            .await
            .unwrap_or_else(|e| panic!("清理失败，请手动删除片段 {}: {e}", target.id));
        for dir in [&dir_a, &dir_b] {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}
