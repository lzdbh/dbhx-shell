//! 云同步快照与合并：纯数据、纯逻辑，不碰磁盘也不碰密钥。
//!
//! 分成两层的理由：合并规则是整个功能里最容易写错、又最难在单机上发现错的部分
//! （它只在「第二台设备」上表现），所以这里让它只依赖结构体和数据，
//! 一张表就能测完；真正改 vault 文档的动作留在 `store` 里执行本模块产出的计划。
//!
//! 一条贯穿全部规则的不变量，是后面多处简化的前提：
//! **记录与密文条目的内容一旦写入就不再原地修改**——改口令是新建一条密文并换引用
//! （`Vault::set_host_password`），改主机是整条覆盖并推 `updated_at_ms`。
//! 所以「同一个 id」就等于「同一份内容」：导入时看到本地已有同 id 的密文可以直接跳过，
//! 判断两条记录是否一致也可以直接比字节。

use serde::{Deserialize, Serialize};

use dbhx_model::{AuthMode, HostId, HostRecord, IdentityId, IdentityRecord};

use crate::store::{DeletionKind, DeletionMark, SecretKind};

/// 快照结构版本。与 vault 文档版本是两回事：文档升版不必然要动快照格式。
const SNAPSHOT_VERSION: u32 = 1;

/// 冲突副本的名字后缀。写死一句话，UI 和测试都按它判断。
pub const CONFLICT_SUFFIX: &str = "（冲突副本）";

/// 快照里的一条密文：种类 + 用**同步密钥**（不是本机主密钥）重新密封后的 base64。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSecret {
    pub kind: SecretKind,
    /// base64(nonce||ciphertext||tag)
    pub cipher: String,
}

/// 一次同步的完整内容。注意它**不含任何明文**：里面的密文只是换了把锁，
/// 拿到这份 JSON 而没有同步串的人，和本地拿到 `vault.json` 而没有 `master.key` 的人一样无助。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSnapshot {
    pub version: u32,
    /// 产出这份快照的设备。平局时用它定序，见 `merge_one`。
    pub device_id: String,
    /// 产出时刻。记录级的先后看各自的 `updated_at_ms`，这个只用于展示「上次同步于」。
    /// 刻意不拿它做任何判定：机器时钟不可信，两台设备的时间不能假设对齐。
    pub made_at_ms: u64,
    /// 全部主机，**包括用私钥认证的那些**。不勾「同步私钥」时少的是身份和钥匙密文，
    /// 主机记录照旧带着 `identity_id`：这样两台设备对同一条主机的字节完全一致，
    /// 不会把「B 手上没这把钥匙」误判成「B 改过这条主机」。
    pub hosts: Vec<HostRecord>,
    pub identities: Vec<IdentityRecord>,
    /// key 是密文 id。只含被上面留下的记录引用到的那些；`lock_pin` 永不在其中。
    pub secrets: Vec<(String, SnapshotSecret)>,
    /// 删除标记。没有它，A 上删掉的主机会从 B 的快照里复活。
    pub tombstones: Vec<DeletionMark>,
}

impl SyncSnapshot {
    pub fn new(device_id: String, made_at_ms: u64) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            device_id,
            made_at_ms,
            hosts: Vec::new(),
            identities: Vec::new(),
            secrets: Vec::new(),
            tombstones: Vec::new(),
        }
    }

    /// 快照按版本分派。目前只有 v1，且 v1 的布局一旦发布就不再动。
    /// 比本应用新的直接拒绝，而不是「尽力解解看」：解错了会报成「同步串不对」，
    /// 那是最难查的一类事故。
    pub fn check_version(&self) -> Result<(), String> {
        if self.version > SNAPSHOT_VERSION {
            return Err(format!(
                "快照版本 {} 比本应用支持的 {SNAPSHOT_VERSION} 新，请升级应用",
                self.version
            ));
        }
        Ok(())
    }

    pub fn secret(&self, id: &str) -> Option<&SnapshotSecret> {
        self.secrets.iter().find(|(k, _)| k == id).map(|(_, v)| v)
    }
}

/// 用户选的合并口径。首次两边都有数据、又没有共同基线时必须由人来定，
/// 因为那种情况下**没有任何算法线索**能分辨「A 删了它」和「B 还没建它」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePolicy {
    /// 逐条按基线与时间定序，败方一律转存成「冲突副本」，一条都不静默丢
    ThreeWay,
    /// 整库以本地为准：什么都不改，随后把本地这份推上去覆盖云端
    LocalWins,
    /// 同 id 的冲突以远端为准，本地被覆盖的内容仍转存为副本
    RemoteWins,
}

/// 合并的三方输入。`base` 是上一次推送/拉取的那份快照解出来的内容；
/// `None` 表示本机从没同步过，那时无从判断「这条是谁改的」，只能按 `policy` 走。
pub struct MergeInput<'a> {
    pub local_hosts: &'a [HostRecord],
    pub local_identities: &'a [IdentityRecord],
    pub local_tombstones: &'a [DeletionMark],
    pub local_device_id: &'a str,
    /// 本机已有的密文 id。远端引用了本地没有的才需要导入。
    pub local_secret_ids: &'a [String],
    pub remote: &'a SyncSnapshot,
    pub base: Option<&'a SyncSnapshot>,
    pub policy: MergePolicy,
}

/// 合并结论：一份「要怎么改本地」的清单。执行它的活儿在 `store` 里。
#[derive(Debug, Clone, Default)]
pub struct MergePlan {
    /// 要新插入或覆盖的主机（含转存的冲突副本，它们的 id 是新生成的）
    pub hosts_upsert: Vec<HostRecord>,
    /// 要从本地删除的主机 id
    pub hosts_remove: Vec<HostId>,
    pub identities_upsert: Vec<IdentityRecord>,
    pub identities_remove: Vec<IdentityId>,
    /// 需要重密封入库的密文（一定都是本地还没有的 id）
    pub secrets_import: Vec<(String, SnapshotSecret)>,
    /// 要并入本地的删除标记（两边的并集，同 id 取更早的时间）
    pub tombstones: Vec<DeletionMark>,
    /// 转存为冲突副本的条目名字，给 UI 一句「有 N 条冲突已另存」
    pub conflicts: Vec<String>,
    /// 认证方式要私钥、但合并完本机仍没有对应身份的主机名。UI 必须逐条念给用户听。
    pub missing_identities: Vec<String>,
    /// 口令密文既不在本地也不在云端快照里的主机名——得重新设一次口令
    pub missing_credentials: Vec<String>,
}

impl MergePlan {
    /// 「这次同步不需要改动本地」。`store` 用它决定是否要置 dirty。
    pub fn is_empty(&self) -> bool {
        self.hosts_upsert.is_empty()
            && self.hosts_remove.is_empty()
            && self.identities_upsert.is_empty()
            && self.identities_remove.is_empty()
            && self.secrets_import.is_empty()
            && self.tombstones.is_empty()
            && self.conflicts.is_empty()
    }
}

/// 一类记录在合并里的通用接口。主机与身份字段不同但规则一模一样，
/// 所以抽出来而不是复制两遍——复制的两遍一定会在改了其中一遍之后不一致。
trait Versioned: Clone {
    fn id_str(&self) -> &str;
    fn updated_ms(&self) -> u64;
    fn label(&self) -> String;
    /// 内容是否一致。两类记录都 derive 了 `PartialEq`，比字节就够。
    /// 时间戳相同不代表内容相同，所以「一致」必须是独立一问。
    fn same_content(&self, other: &Self) -> bool;
    /// 造冲突副本用：换一把新 id、名字加后缀。
    /// 副本**必须换 id**：留着同 id，下一次合并会把它和原条目当成同一条，
    /// 「两条都保留」的意图立刻退化成又一次互相覆盖。
    fn retarget(&mut self, id: String, label: String);
    /// 这条记录引用到的密文 id（口令 / 私钥 / passphrase）
    fn secret_ids(&self) -> Vec<String>;
    /// 落进计划的哪一格
    fn write_into(&self, plan: &mut MergePlan);
    fn remove_into(id_str: &str, plan: &mut MergePlan);
    /// 这条记录还引用着一个外部对象（主机的 identity_id），可能是悬空引用
    fn external_ref(&self) -> Option<String>;
}

impl Versioned for HostRecord {
    fn id_str(&self) -> &str {
        &self.id.0
    }
    fn updated_ms(&self) -> u64 {
        self.updated_at_ms
    }
    fn label(&self) -> String {
        self.label.clone()
    }
    fn same_content(&self, other: &Self) -> bool {
        self == other
    }
    fn retarget(&mut self, id: String, label: String) {
        self.id = HostId(id);
        self.label = label;
    }
    fn secret_ids(&self) -> Vec<String> {
        self.password_secret_id.iter().map(|s| s.0.clone()).collect()
    }
    fn write_into(&self, plan: &mut MergePlan) {
        plan.hosts_upsert.push(self.clone());
    }
    fn remove_into(id_str: &str, plan: &mut MergePlan) {
        plan.hosts_remove.push(HostId(id_str.to_string()));
    }
    fn external_ref(&self) -> Option<String> {
        // 只有真按私钥认证的主机才需要身份；其余模式下 identity_id 是残留，不该报警
        match self.auth_mode {
            AuthMode::PublicKey => self.identity_id.as_ref().map(|i| i.0.clone()),
            _ => None,
        }
    }
}

impl Versioned for IdentityRecord {
    fn id_str(&self) -> &str {
        &self.id.0
    }
    fn updated_ms(&self) -> u64 {
        self.updated_at_ms
    }
    fn label(&self) -> String {
        self.label.clone()
    }
    fn same_content(&self, other: &Self) -> bool {
        self == other
    }
    fn retarget(&mut self, id: String, label: String) {
        self.id = IdentityId(id);
        self.label = label;
    }
    fn secret_ids(&self) -> Vec<String> {
        let mut ids = vec![self.private_key_secret_id.0.clone()];
        ids.extend(self.passphrase_secret_id.iter().map(|s| s.0.clone()));
        ids
    }
    fn write_into(&self, plan: &mut MergePlan) {
        plan.identities_upsert.push(self.clone());
    }
    fn remove_into(id_str: &str, plan: &mut MergePlan) {
        plan.identities_remove.push(IdentityId(id_str.to_string()));
    }
    fn external_ref(&self) -> Option<String> {
        None
    }
}

/// 单个 id 在一类记录里的合并结论。
#[derive(Debug)]
struct Merged<T> {
    /// 这条 id 最终应有的内容；`None` 表示它不该存在（删除胜出，或两边都没有）
    keep: Option<T>,
    /// 败方转存的副本。只在「两边都改过」或「删除胜出但另一边有实质的编辑」时出现。
    copies: Vec<T>,
}

fn tombstone_of<'a>(marks: &'a [DeletionMark], id: &str, kind: DeletionKind) -> Option<&'a DeletionMark> {
    marks.iter().find(|t| t.kind == kind && t.id == id)
}

/// 同 id 的删除标记取两边更早的那个：「第一次删掉」才是别的设备要听的时间戳，
/// 各自重复删除是本机的操作噪声，不该把删除时刻往后推。
fn earlier<'a>(a: &'a DeletionMark, b: &'a DeletionMark) -> &'a DeletionMark {
    if a.deleted_at_ms <= b.deleted_at_ms {
        a
    } else {
        b
    }
}

fn find_by_id<'a, T: Versioned>(v: &'a [T], id: &str) -> Option<&'a T> {
    v.iter().find(|r| r.id_str() == id)
}

/// 逐条判定。规则表（`t` = 该 id 的删除标记）：
///
/// 1. **有 `t` ⇒ 删除胜出**。能出现「同 id 又活了」只有一种情况：另一台设备上还没收到
///    这次删除的旧数据。用户在本地重新添加同一台机器会拿到全新 id，与这条墓碑无关。
///    但如果那一边的记录与基线**内容不同**，说明那是一次真实的编辑，转存成副本，不静默丢。
/// 2. **无 `t`，两边都活着**：字节一致就什么都不做；否则
///    - 有基线：和基线一致的那边就是没动过的，听另一个的（这是绝大多数真实场景）；
///    - 两边都改了、或压根没有基线：按 `policy`。`ThreeWay` 用 `updated_at_ms` 定序，
///      完全同秒时才比 device_id——不是「谁更重要」，而是要让两台设备各自离线跑同一次
///      合并也能得到**同一个**结论，否则下一轮同步又是一次冲突。
/// 3. **无 `t`，只有一边有**：保留有的那边。远端新增就导入，本地独有就留着——
///    连「以远端为准」也不删本地独有记录：那批是这台设备刚建、还没推上去的东西，
///    删它们等于把「以远端为准」变成「丢掉本机今天的劳动」。
fn merge_one<T: Versioned>(
    l: Option<&T>,
    r: Option<&T>,
    b: Option<&T>,
    tomb: Option<&DeletionMark>,
    policy: MergePolicy,
    local_device: &str,
    remote_device: &str,
) -> Merged<T> {
    if tomb.is_some() {
        let mut copies = Vec::new();
        // 只有与基线不同（= 有实质编辑）的那一边才值得留副本；和基线一样的那份是旧状态
        for side in l.into_iter().chain(r) {
            let edited = b.map(|base| !side.same_content(base)).unwrap_or(true);
            if edited {
                copies.push(side.clone());
            }
        }
        return Merged { keep: None, copies };
    }
    match (l, r) {
        (Some(a), Some(c)) if a.same_content(c) => Merged {
            keep: Some(a.clone()),
            copies: Vec::new(),
        },
        (Some(a), Some(c)) => {
            let take_remote = match policy {
                MergePolicy::RemoteWins => true,
                MergePolicy::ThreeWay => match a.updated_ms().cmp(&c.updated_ms()) {
                    std::cmp::Ordering::Equal => remote_device > local_device,
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Less => true,
                },
                // 调用方在 LocalWins 时不会走到这里，plan_merge 直接返回空计划
                MergePolicy::LocalWins => false,
            };
            let winner = if take_remote { c } else { a };
            let loser = if take_remote { a } else { c };
            let mut copies = Vec::new();
            // 败方若与基线一致，说明它压根没动过，不配占一个副本位
            if !b.map(|base| loser.same_content(base)).unwrap_or(false) {
                copies.push(loser.clone());
            }
            Merged {
                keep: Some(winner.clone()),
                copies,
            }
        }
        (Some(a), None) => Merged {
            keep: Some(a.clone()),
            copies: Vec::new(),
        },
        (None, Some(c)) => Merged {
            keep: Some(c.clone()),
            copies: Vec::new(),
        },
        (None, None) => Merged {
            keep: None,
            copies: Vec::new(),
        },
    }
}

/// 产出一份合并计划。这是同步功能的核心判定，`store` 只负责执行它。
///
/// `new_id` 由调用方给（`store::new_id`）：冲突副本要换 id，而 id 的生成规则
/// 属于 vault，不属于这个纯逻辑模块。
pub fn plan_merge(input: &MergeInput, new_id: impl Fn() -> String) -> MergePlan {
    // 「以本地为准」= 不动本地。墓碑也不执行：用户要的是拿本地这份去覆盖云端，
    // 而不是让云端那批删除标记反过来删本地。
    if input.policy == MergePolicy::LocalWins {
        return MergePlan::default();
    }
    let mut plan = MergePlan::default();
    let base_hosts = input.base.map(|s| &s.hosts[..]).unwrap_or(&[]);
    let base_identities = input.base.map(|s| &s.identities[..]).unwrap_or(&[]);
    // 先合身份再合主机：主机的悬空身份判定要看到身份的结果。
    // 反过来（先主机）会让「远端同时带来一把新钥匙和引用它的主机」被误报成缺身份。
    collect(input.local_identities, &input.remote.identities, base_identities, DeletionKind::Identity, input, &new_id, &mut plan);
    collect(input.local_hosts, &input.remote.hosts, base_hosts, DeletionKind::Host, input, &new_id, &mut plan);
    // 删除标记：两边并集，同 id 取更早的
    for mark in input.local_tombstones.iter().chain(input.remote.tombstones.iter()) {
        let picked = mark.clone();
        match plan
            .tombstones
            .iter_mut()
            .find(|t| t.kind == picked.kind && t.id == picked.id)
        {
            Some(existing) => {
                let keep = earlier(existing, &picked).clone();
                *existing = keep;
            }
            None => plan.tombstones.push(picked),
        }
    }
    // 悬空身份：合并完仍解析不到的 identity_id。只查**这次写进来的**主机——
    // 本地早就存在的悬空引用是上一次操作的事，不该由同步来报。
    let resolved: Vec<&str> = input
        .local_identities
        .iter()
        .map(|i| i.id_str())
        .chain(plan.identities_upsert.iter().map(|i| i.id_str()))
        .collect();
    for host in &plan.hosts_upsert {
        if let Some(need) = host.external_ref() {
            if !resolved.iter().any(|r| *r == need) {
                plan.missing_identities.push(host.label.clone());
            }
        }
        // 口令密文在两边都找不到：这条主机同步过来也连不上，必须重设口令
        for sid in host.secret_ids() {
            if !input.local_secret_ids.iter().any(|l| *l == sid) && input.remote.secret(&sid).is_none() {
                plan.missing_credentials.push(host.label.clone());
            }
        }
    }
    plan
}

/// 一类记录的合并：逐 id 判定，把结论写成对本地文档的操作，顺带收集需要导入的密文。
fn collect<T: Versioned>(
    local: &[T],
    remote: &[T],
    base: &[T],
    kind: DeletionKind,
    input: &MergeInput,
    new_id: &impl Fn() -> String,
    plan: &mut MergePlan,
) {
    // 只遍历「两边至少有一边有记录」的 id：只在基线里出现过的说明两边都已经没有它了，
    // 那是墓碑的事，跟记录无关。
    let mut ids: Vec<&str> = Vec::with_capacity(local.len() + remote.len());
    for r in local.iter().chain(remote.iter()) {
        if !ids.contains(&r.id_str()) {
            ids.push(r.id_str());
        }
    }
    for id in ids {
        let l = find_by_id(local, id);
        let r = find_by_id(remote, id);
        let b = find_by_id(base, id);
        let tomb = match (
            tombstone_of(input.local_tombstones, id, kind),
            tombstone_of(&input.remote.tombstones, id, kind),
        ) {
            (Some(a), Some(c)) => Some(earlier(a, c)),
            (Some(a), None) => Some(a),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        };
        let merged = merge_one(l, r, b, tomb, input.policy, input.local_device_id, &input.remote.device_id);
        // 结论与本地现状不一致才要写；本地已经没有的（远端新增）也走这条
        match (&merged.keep, l) {
            (Some(want), Some(have)) if want.same_content(have) => {}
            (Some(want), _) => {
                want.write_into(plan);
                need_secrets(want, input, plan);
            }
            (None, Some(_)) => T::remove_into(id, plan),
            (None, None) => {}
        }
        for copy in merged.copies {
            let mut copy = copy;
            let label = format!("{}{CONFLICT_SUFFIX}", copy.label());
            copy.retarget(new_id(), label.clone());
            copy.write_into(plan);
            need_secrets(&copy, input, plan);
            plan.conflicts.push(label);
        }
    }
}

/// 一条要写进本地的记录所引用的密文里，哪些还得从远端导入。
///
/// 本地已有同 id 就跳过——依据是模块头那条不变量：id 相同即内容相同。
/// 远端也没有的（比如只勾了同步私钥、而这把身份是别人删掉的）不报错，
/// 由调用方按 `missing_*` 清单交代给用户。
fn need_secrets<T: Versioned>(record: &T, input: &MergeInput, plan: &mut MergePlan) {
    for sid in record.secret_ids() {
        if input.local_secret_ids.iter().any(|l| *l == sid) {
            continue;
        }
        if plan.secrets_import.iter().any(|(k, _)| *k == sid) {
            continue;
        }
        if let Some(secret) = input.remote.secret(&sid) {
            plan.secrets_import.push((sid, secret.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::{Endpoint, Protocol, SecretId, TerminalEncoding};

    fn host(id: &str, label: &str, updated: u64) -> HostRecord {
        HostRecord {
            id: HostId(id.into()),
            label: label.into(),
            group: None,
            tags: vec![],
            favorite: false,
            protocol: Protocol::Ssh,
            os: dbhx_model::OsType::Linux,
            endpoint: Endpoint {
                host: "10.0.0.1".into(),
                port: Some(22),
                username: "app".into(),
            },
            auth_mode: AuthMode::Password,
            identity_id: None,
            password_secret_id: Some(SecretId(format!("s-{id}"))),
            keepalive_secs: 0,
            encoding: TerminalEncoding::Utf8,
            created_at_ms: 1,
            updated_at_ms: updated,
        }
    }

    fn mark(id: &str, kind: DeletionKind, at: u64, device: &str) -> DeletionMark {
        DeletionMark {
            id: id.into(),
            kind,
            deleted_at_ms: at,
            device_id: device.into(),
        }
    }

    fn snapshot(device: &str, hosts: Vec<HostRecord>) -> SyncSnapshot {
        let mut snap = SyncSnapshot::new(device.into(), 1_000);
        snap.secrets = hosts
            .iter()
            .flat_map(|h| h.secret_ids())
            .map(|id| {
                (
                    id.clone(),
                    SnapshotSecret {
                        kind: SecretKind::Password,
                        cipher: format!("cipher-of-{id}"),
                    },
                )
            })
            .collect();
        snap.hosts = hosts;
        snap
    }

    /// 把 plan_merge 的输入拼起来。本地密文 id 按主机的引用现造，模拟「本地都有」。
    struct Case {
        local: Vec<HostRecord>,
        remote: SyncSnapshot,
        base: Option<SyncSnapshot>,
        local_tombstones: Vec<DeletionMark>,
        policy: MergePolicy,
    }

    fn run(case: Case) -> MergePlan {
        let local_ids: Vec<String> = case
            .local
            .iter()
            .flat_map(|h| h.secret_ids())
            .chain(case.local.iter().flat_map(|h| h.password_secret_id.iter().map(|s| s.0.clone())))
            .collect();
        let input = MergeInput {
            local_hosts: &case.local,
            local_identities: &[],
            local_tombstones: &case.local_tombstones,
            local_device_id: "AAA",
            local_secret_ids: &local_ids,
            remote: &case.remote,
            base: case.base.as_ref(),
            policy: case.policy,
        };
        plan_merge(&input, || "newid".into())
    }

    #[test]
    fn a_record_only_on_the_remote_is_imported_with_its_credential() {
        let plan = run(Case {
            local: vec![],
            remote: snapshot("BBB", vec![host("h1", "新机器", 10)]),
            base: None,
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_upsert.len(), 1);
        assert_eq!(plan.secrets_import.len(), 1, "导入主机必须连带把它的口令密文搬进来");
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn identical_records_on_both_sides_produce_no_writes() {
        let h = host("h1", "同一台", 10);
        let plan = run(Case {
            local: vec![h.clone()],
            remote: snapshot("BBB", vec![h]),
            base: None,
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert!(plan.hosts_upsert.is_empty(), "字节一致就不该有任何写入");
        assert!(plan.secrets_import.is_empty());
    }

    #[test]
    fn only_one_side_changed_wins_without_a_conflict_copy() {
        // 这是最常见的真实场景：A 改了口令，B 没动
        let base = host("h1", "老名字", 10);
        let changed = HostRecord {
            label: "改过名字".into(),
            updated_at_ms: 20,
            ..base.clone()
        };
        let plan = run(Case {
            local: vec![base.clone()],
            remote: snapshot("BBB", vec![changed.clone()]),
            base: Some(snapshot("CCC", vec![base])),
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_upsert, vec![changed]);
        assert!(plan.conflicts.is_empty(), "只有一边动过，不该造副本");
    }

    #[test]
    fn both_changed_keeps_the_newer_and_preserves_the_loser_as_a_copy() {
        let base = host("h1", "老名字", 10);
        let local = HostRecord {
            label: "本地改的".into(),
            updated_at_ms: 20,
            ..base.clone()
        };
        let remote = HostRecord {
            label: "远端改的".into(),
            updated_at_ms: 30,
            ..base.clone()
        };
        let plan = run(Case {
            local: vec![local],
            remote: snapshot("BBB", vec![remote]),
            base: Some(snapshot("CCC", vec![base])),
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_upsert.len(), 2, "胜方 + 一份冲突副本");
        assert_eq!(plan.hosts_upsert[0].label, "远端改的");
        assert_eq!(plan.hosts_upsert[1].label, "本地改的（冲突副本）");
        assert_ne!(plan.hosts_upsert[0].id, plan.hosts_upsert[1].id, "副本必须换新 id，否则下一轮又互相覆盖");
        assert_eq!(plan.conflicts, vec!["本地改的（冲突副本）".to_string()]);
    }

    #[test]
    fn a_locally_newer_edit_wins_and_only_the_remote_side_becomes_a_copy() {
        // 胜方就是本地现状时不该有任何「写回自己」的动作，否则每次同步都置 dirty
        let base = host("h1", "老名字", 10);
        let local = HostRecord {
            label: "本地改的".into(),
            updated_at_ms: 30,
            ..base.clone()
        };
        let remote = HostRecord {
            label: "远端改的".into(),
            updated_at_ms: 20,
            ..base.clone()
        };
        let plan = run(Case {
            local: vec![local.clone()],
            remote: snapshot("BBB", vec![remote]),
            base: Some(snapshot("CCC", vec![base])),
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_upsert.len(), 1, "只有落败的那一侧需要写入");
        assert_eq!(plan.hosts_upsert[0].label, "远端改的（冲突副本）");
        assert_eq!(plan.conflicts, vec!["远端改的（冲突副本）".to_string()]);
    }

    #[test]
    fn same_second_conflict_is_broken_by_device_id_not_by_coin_flipping() {
        let base = host("h1", "老名字", 10);
        let local = HostRecord {
            label: "本地".into(),
            updated_at_ms: 20,
            ..base.clone()
        };
        let remote = HostRecord {
            label: "远端".into(),
            updated_at_ms: 20,
            ..base.clone()
        };
        // 本地 device_id 固定是 "AAA"，远端 "BBB" > "AAA" ⇒ 远端胜。
        // 同一次数据在两台设备上各跑一遍必须得出同一个结论，所以定序只能来自数据本身。
        let plan = run(Case {
            local: vec![local],
            remote: snapshot("BBB", vec![remote]),
            base: Some(snapshot("CCC", vec![base])),
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_upsert[0].label, "远端");
    }

    #[test]
    fn a_deletion_beats_a_stale_record_and_keeps_a_real_edit_as_a_copy() {
        // 远端删了 h1；本地这条既没被改过（=基线）又留着 ⇒ 直接删掉，不留副本
        let base = host("h1", "老名字", 10);
        let plan = run(Case {
            local: vec![base.clone()],
            remote: snapshot("BBB", vec![]),
            base: Some(snapshot("CCC", vec![base.clone()])),
            local_tombstones: vec![mark("h1", DeletionKind::Host, 20, "BBB")],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_remove, vec![HostId("h1".into())]);
        assert!(plan.conflicts.is_empty(), "没动过的旧状态不值得占一个副本位");

        // 本地在它之后真的编辑过 ⇒ 删除仍然胜出，但编辑要留副本
        let edited = HostRecord {
            label: "本地删后改的".into(),
            updated_at_ms: 30,
            ..base.clone()
        };
        let plan = run(Case {
            local: vec![edited],
            remote: snapshot("BBB", vec![]),
            base: Some(snapshot("CCC", vec![base])),
            local_tombstones: vec![mark("h1", DeletionKind::Host, 20, "BBB")],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.hosts_remove, vec![HostId("h1".into())]);
        assert_eq!(plan.conflicts, vec!["本地删后改的（冲突副本）".to_string()]);
        assert_eq!(plan.hosts_upsert.len(), 1);
        assert_eq!(plan.hosts_upsert[0].id, HostId("newid".into()));
    }

    #[test]
    fn local_wins_policy_touches_nothing_even_against_remote_tombstones() {
        let local = host("h1", "只有我有", 10);
        let plan = run(Case {
            local: vec![local.clone()],
            remote: snapshot("BBB", vec![host("h2", "云端独有", 99)]),
            base: None,
            local_tombstones: vec![],
            policy: MergePolicy::LocalWins,
        });
        assert!(plan.is_empty(), "以本地为准 = 什么都不改，随后拿本地去覆盖云端");
    }

    #[test]
    fn remote_wins_overwrites_same_id_but_never_deletes_local_only_records() {
        let local_only = host("h9", "今天刚建还没推", 5);
        let base = host("h1", "老名字", 10);
        let local = HostRecord {
            label: "本地".into(),
            updated_at_ms: 30,
            ..base.clone()
        };
        let remote = HostRecord {
            label: "远端".into(),
            updated_at_ms: 11,
            ..base.clone()
        };
        let plan = run(Case {
            local: vec![local, local_only],
            remote: snapshot("BBB", vec![remote.clone()]),
            base: Some(snapshot("CCC", vec![base])),
            local_tombstones: vec![],
            policy: MergePolicy::RemoteWins,
        });
        // 远端更旧也照覆盖（用户就是要远端那份），但本地那条编辑仍留副本
        assert_eq!(plan.hosts_upsert[0], remote);
        assert_eq!(plan.conflicts, vec!["本地（冲突副本）".to_string()]);
        assert!(
            plan.hosts_remove.is_empty(),
            "「以远端为准」不能删本地独有的记录"
        );
    }

    #[test]
    fn tombstones_merge_to_the_union_with_the_earliest_time() {
        let plan = run(Case {
            local: vec![],
            remote: SyncSnapshot {
                tombstones: vec![mark("h5", DeletionKind::Host, 40, "BBB"), mark("h1", DeletionKind::Host, 15, "BBB")],
                ..snapshot("BBB", vec![])
            },
            base: None,
            local_tombstones: vec![mark("h1", DeletionKind::Host, 8, "AAA")],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.tombstones.len(), 2);
        let h1 = plan.tombstones.iter().find(|t| t.id == "h1").unwrap();
        assert_eq!(h1.deleted_at_ms, 8, "同一条删除要取更早的那次，否则删除时刻会被重复操作推后");
        assert_eq!(h1.device_id, "AAA");
    }

    #[test]
    fn a_host_whose_key_was_not_synced_is_reported_not_broken() {
        // 不勾「同步私钥」时：主机带着 identity_id 过来了，身份却没过来
        let mut h = host("h1", "用钥匙的那台", 10);
        h.auth_mode = AuthMode::PublicKey;
        h.identity_id = Some(dbhx_model::IdentityId("k1".into()));
        h.password_secret_id = None;
        let plan = run(Case {
            local: vec![],
            remote: snapshot("BBB", vec![h]),
            base: None,
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.missing_identities, vec!["用钥匙的那台".to_string()]);
        assert!(
            plan.missing_credentials.is_empty(),
            "私钥认证本来就没有口令密文，不该报成缺凭据"
        );
    }

    #[test]
    fn a_credential_missing_on_both_sides_is_reported() {
        let mut snap = snapshot("BBB", vec![host("h1", "口令丢了", 10)]);
        snap.secrets.clear(); // 模拟云端这份快照里就是没有这条密文
        let plan = run(Case {
            local: vec![],
            remote: snap,
            base: None,
            local_tombstones: vec![],
            policy: MergePolicy::ThreeWay,
        });
        assert_eq!(plan.missing_credentials, vec!["口令丢了".to_string()]);
        assert!(plan.secrets_import.is_empty());
    }

    #[test]
    fn version_newer_than_this_app_is_refused_rather_than_guessed() {
        let mut snap = snapshot("BBB", vec![]);
        snap.version = SNAPSHOT_VERSION + 1;
        assert!(snap.check_version().is_err(), "新格式必须硬拒绝：解错了会报成「同步串不对」");
        snap.version = SNAPSHOT_VERSION;
        assert!(snap.check_version().is_ok());
    }
}
