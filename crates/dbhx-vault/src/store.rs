//! Vault 文档与持久化。
//!
//! 一整个加密 JSON 文档（不是每条记录一个文件）：主机数量在几百级，
//! 整文档读写换来的是「一次 rename 就是原子提交」，不用自己设计并发追加日志。
//! 明文秘密只在 `with_secret` 的回调里存在，回调外拿到的是 `Zeroizing`，出作用域清零。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dbhx_model::{HostId, HostRecord, IdentityId, IdentityRecord, SecretId, SyncTarget};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{open, seal, KeySource, MasterKey};
use crate::error::VaultError;
use crate::snapshot::{MergePlan, SnapshotSecret, SyncSnapshot};
use crate::sync::DerivedKey;

/// 当前文档结构版本。加字段时递增并写迁移，读到老版本要能升上来。
///
/// v1 → v2（2026-10-15，为云同步做的改动）：加 `tombstones` 与 `device_id`。
/// 没有删除标记的话，A 上删掉的主机会从 B 上次同步的快照里原样复活——
/// 这是同步功能里最容易被当成「云端坏了」的那类 bug，而且只在第二台设备上出现。
const DOC_VERSION: u32 = 2;

/// 删除标记挂在哪一类记录上。合并时按 `(kind, id)` 配对。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionKind {
    Host,
    Identity,
}

/// 一条删除标记。被删记录的真实内容已经没了，这里只留「谁在什么时候删的」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionMark {
    /// 被删记录的 id
    pub id: String,
    pub kind: DeletionKind,
    pub deleted_at_ms: u64,
    /// 哪台设备删的。两边在同一毫秒各删各的时用它在设备间定序，
    /// 也让「这台机器什么时候没了这台主机」在本地可追溯。
    pub device_id: String,
}

/// 删除标记的保留窗口：90 天。
///
/// 必须有窗口，因为标记是「防复活」的，而一台离线三个月的设备重新联网时，
/// 它本地那份老数据里还带着那台主机。窗口一到就过期删除——这是有意的取舍：
/// 没有任何上限的墓碑会让文档无限增长，而三个月没上线的设备本来也不该
/// 指望它的老清单还能覆盖别人。
const TOMBSTONE_RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// 密文条目的种类。存进文档是为了在「删除前提示」和 UI 徽章里区分口令与私钥，
/// 但种类本身不是秘密，可以进日志。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    Password,
    PrivateKey,
    Passphrase,
    /// 应用锁口令。它和其它几种一样只是密文条目，但用途是「校验」而不是「交给调用方」：
    /// 出库只有 `check_lock_pin` 的布尔结果，永远不会把明文带出去。
    LockPin,
    /// 云同步的同步串。存进 vault 只是为了免用户每次重贴，
    /// 它和 `LockPin` 同一类：不参与同步、不参与重密封，但必须被引用计数保住。
    SyncStr,
    /// 云端服务的访问令牌（GitHub / Gitee）。和上面两种同一条规则：
    /// 只被引用计数保住，绝不进云端快照，跨 IPC 也只报「配过没有」。
    ProviderToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SecretEntry {
    kind: SecretKind,
    /// base64(nonce||ciphertext||tag)
    cipher: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VaultDoc {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default)]
    hosts: Vec<HostRecord>,
    #[serde(default)]
    identities: Vec<IdentityRecord>,
    #[serde(default)]
    secrets: BTreeMap<String, SecretEntry>,
    /// 应用锁口令的密文条目。`None` 表示没设过锁——老库没有这个字段，
    /// `serde(default)` 让它们在升级后照常打开。
    #[serde(default)]
    lock_pin: Option<SecretId>,
    /// 删除标记。见 `DeletionMark`：没有它，删掉的主机会在另一台设备上复活。
    #[serde(default)]
    tombstones: Vec<DeletionMark>,
    /// 本机安装标识，首次写盘时生成后固定不变。合并时跨设备定序用它。
    #[serde(default)]
    device_id: String,
    /// 云同步串的密文条目。`None` 表示这台设备还没配过同步。
    #[serde(default)]
    sync_str: Option<SecretId>,
    /// 云端令牌的密文条目。和 `sync_str` 分开存：换令牌不该把同步串也弄丢，
    /// 反过来配新令牌时也不该逼用户重贴那串 32 位字符。
    #[serde(default)]
    provider_token: Option<SecretId>,
    /// 云端那一份的坐标（哪家服务 + 哪个片段）。明文存：它本身不是秘密，
    /// 见 `dbhx_model::SyncTarget`。
    #[serde(default)]
    sync_target: Option<SyncTarget>,
    /// 上一次同步完成时**本地这份快照**的副本，用本机主密钥密封。
    ///
    /// 它是三方合并的**基线**：没有基线就只能整库定胜负，那正是合并最容易
    /// 默默丢数据的情形。之所以存快照而不是云端那份 blob：blob 得用同步密钥才解得开，
    /// 而合并每次同步都要读基线一回 —— 存 blob 等于每次同步多付一次 Argon2id（实测 5s）。
    /// 换成用本机密钥封的快照之后，读基线跟读一条普通密文一样便宜。
    /// 安全性没有额外损失：拿到它和拿到云端那一份等价，都需要同步串才能解。
    #[serde(default)]
    sync_base: Option<String>,
}

fn current_version() -> u32 {
    DOC_VERSION
}

impl VaultDoc {
    /// 把老版本升到当前结构。返回**是否改动过**，改动意味着必须落盘——
    /// `device_id` 这类字段每进程换一个，跨设备定序就成了掷骰子。
    ///
    /// 迁移必须逐级做（v1→v2→v3…），不能一步跳：将来加 v3 时，
    /// 这里要保证 v1 的文件也能把 v2 该补的东西补上。
    fn migrate_to_current(&mut self) -> bool {
        let mut changed = false;
        if self.version < 2 {
            if self.device_id.is_empty() {
                self.device_id = new_id();
                changed = true;
            }
        }
        if self.version != DOC_VERSION {
            self.version = DOC_VERSION;
            changed = true;
        }
        changed
    }
}

/// 加密 vault。`path` 是文档落盘位置，`dirty` 让「只读查询」不必重写文件。
#[derive(Debug)]
pub struct Vault {
    path: PathBuf,
    key: MasterKey,
    doc: VaultDoc,
    dirty: bool,
}

impl Vault {
    /// 打开（不存在则新建空库）。文件损坏时**不覆盖**：
    /// 一次误覆盖就是整库凭据丢失，宁可让人来处理。
    pub fn open(dir: &Path, key_source: KeySource) -> Result<Self, VaultError> {
        let key = key_source.load_or_create()?;
        let path = dir.join("vault.json");
        std::fs::create_dir_all(dir).map_err(|e| VaultError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let vault = if path.exists() {
            let raw = std::fs::read(&path).map_err(|e| VaultError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
            // 落盘是 base64 文本；非 UTF-8 直接判损坏，不做有损解码
            let text = String::from_utf8(raw)
                .map_err(|_| VaultError::Malformed(format!("{} is not utf-8", path.display())))?;
            let sealed = base64_decode(&text).ok_or_else(|| {
                VaultError::Malformed(format!("{} is not a base64 vault blob", path.display()))
            })?;
            let plain = open(&key, &sealed)?;
            let mut doc: VaultDoc = serde_json::from_slice(&plain)
                .map_err(|e| VaultError::Malformed(format!("vault document: {e}")))?;
            if doc.version > DOC_VERSION {
                return Err(VaultError::Malformed(format!(
                    "vault version {} is newer than supported {}",
                    doc.version, DOC_VERSION
                )));
            }
            // 迁移是有状态的升级：`device_id` 一旦生成就不能换，所以升完必须落盘。
            // 让老库在打开那一刻就置 dirty，而不是等用户改了什么才想起来升版。
            let migrated = doc.migrate_to_current();
            Self {
                path,
                key,
                doc,
                dirty: migrated,
            }
        } else {
            let mut doc = VaultDoc {
                version: DOC_VERSION,
                ..Default::default()
            };
            doc.migrate_to_current();
            Self {
                path,
                key,
                doc,
                // 新建的空库不必急着落盘：等第一次真正有内容时再写
                dirty: false,
            }
        };
        Ok(vault)
    }

    pub fn flush(&mut self) -> Result<(), VaultError> {
        if !self.dirty {
            return Ok(());
        }
        // 清扫放在写盘前而不是打开时：墓碑什么时候算过期取决于「现在」，
        // 在写入路径上做能保证落盘的那份就是干净的，而只读打开不会改动任何东西。
        self.prune_tombstones(now_ms());
        let plain = serde_json::to_vec(&self.doc)
            .map_err(|e| VaultError::Malformed(format!("serialize: {e}")))?;
        let sealed = seal(&self.key, &plain)?;
        let blob = base64_encode(&sealed);
        // 先写 .tmp 再 rename：中途断电只会留下半个 .tmp，原库仍然完好
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, blob.as_bytes()).map_err(|e| VaultError::Io {
            path: tmp.display().to_string(),
            source: e,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|e| VaultError::Io {
            path: self.path.display().to_string(),
            source: e,
        })?;
        self.dirty = false;
        Ok(())
    }

    pub fn hosts(&self) -> &[HostRecord] {
        &self.doc.hosts
    }

    pub fn host(&self, id: &HostId) -> Option<&HostRecord> {
        self.doc.hosts.iter().find(|h| h.id == *id)
    }

    pub fn identities(&self) -> &[IdentityRecord] {
        &self.doc.identities
    }

    /// 本机安装标识。首次打开老库时生成并固定，跨设备合并靠它定序。
    pub fn device_id(&self) -> &str {
        &self.doc.device_id
    }

    /// 当前还留着的删除标记。同步导出时整段带上去，合并时按它判定「删除胜出」。
    pub fn tombstones(&self) -> &[DeletionMark] {
        &self.doc.tombstones
    }

    /// 记一条删除标记。同一 id 重复删只保留最早那次的时间——
    /// 「第一次删掉」才是别的设备要听的时间戳，重复删是本机的操作噪声。
    fn record_deletion(&mut self, id: &str, kind: DeletionKind, at_ms: u64) {
        if let Some(existing) = self
            .doc
            .tombstones
            .iter_mut()
            .find(|t| t.id == id && t.kind == kind)
        {
            if existing.deleted_at_ms > at_ms {
                existing.deleted_at_ms = at_ms;
                existing.device_id = self.doc.device_id.clone();
            }
            return;
        }
        self.doc.tombstones.push(DeletionMark {
            id: id.to_string(),
            kind,
            deleted_at_ms: at_ms,
            device_id: self.doc.device_id.clone(),
        });
    }

    /// 丢弃过期标记。只在 `flush` 里调，见那里的说明。
    fn prune_tombstones(&mut self, now_ms: u64) {
        let keep_before = now_ms.saturating_sub(TOMBSTONE_RETENTION_MS);
        let before = self.doc.tombstones.len();
        self.doc.tombstones.retain(|t| t.deleted_at_ms > keep_before);
        if self.doc.tombstones.len() != before {
            self.dirty = true;
        }
    }

    /// 合并用：把远端带来的删除标记并进来，同 id 取更早的时间。
    fn merge_tombstone(&mut self, mark: &DeletionMark) {
        match self
            .doc
            .tombstones
            .iter_mut()
            .find(|t| t.id == mark.id && t.kind == mark.kind)
        {
            Some(existing) if existing.deleted_at_ms > mark.deleted_at_ms => *existing = mark.clone(),
            Some(_) => {}
            None => self.doc.tombstones.push(mark.clone()),
        }
        self.dirty = true;
    }

    /// 新增或按 id 覆盖一条主机。空 id 视为新增并分配。
    pub fn upsert_host(&mut self, mut record: HostRecord) -> HostId {
        if record.id.0.is_empty() {
            record.id = HostId(new_id());
        }
        record.updated_at_ms = now_ms();
        match self.doc.hosts.iter_mut().find(|h| h.id == record.id) {
            Some(existing) => *existing = record.clone(),
            None => self.doc.hosts.push(record.clone()),
        }
        // 记录又活了，删除标记必须跟着清掉：留着它，下一次合并会把这条判给
        // 「删除胜出」，于是「删掉→重新添加同一台」在别的设备上表现为凭空消失。
        self.doc
            .tombstones
            .retain(|t| !(t.kind == DeletionKind::Host && t.id == record.id.0));
        self.dirty = true;
        record.id
    }

    /// 删主机，并顺手回收**只被它引用**的口令密文。
    pub fn remove_host(&mut self, id: &HostId) -> Result<(), VaultError> {
        let before = self.doc.hosts.len();
        let removed = self
            .doc
            .hosts
            .iter()
            .find(|h| h.id == *id)
            .cloned()
            .ok_or_else(|| VaultError::NotFound(format!("host {}", id.0)))?;
        self.doc.hosts.retain(|h| h.id != *id);
        if self.doc.hosts.len() == before {
            return Err(VaultError::NotFound(format!("host {}", id.0)));
        }
        if let Some(secret) = &removed.password_secret_id {
            self.drop_secret_if_unreferenced(secret);
        }
        self.record_deletion(&id.0, DeletionKind::Host, now_ms());
        self.dirty = true;
        Ok(())
    }

    /// 存一把私钥（可选口令），返回身份记录。字节从这里开始就是 `Zeroizing`，
    /// 调用方传进来的 `&[u8]` 由调用方负责擦除。
    pub fn add_identity(
        &mut self,
        label: &str,
        private_key: &[u8],
        passphrase: Option<&[u8]>,
    ) -> Result<IdentityId, VaultError> {
        let pk_secret = self.put_secret(SecretKind::PrivateKey, private_key)?;
        let pass_secret = match passphrase {
            Some(p) if !p.is_empty() => Some(self.put_secret(SecretKind::Passphrase, p)?),
            _ => None,
        };
        let id = IdentityId(new_id());
        self.doc.identities.push(IdentityRecord {
            id: id.clone(),
            label: label.to_string(),
            private_key_secret_id: pk_secret,
            passphrase_secret_id: pass_secret,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        });
        self.dirty = true;
        Ok(id)
    }

    pub fn remove_identity(&mut self, id: &IdentityId) -> Result<(), VaultError> {
        let record = self
            .doc
            .identities
            .iter()
            .find(|i| i.id == *id)
            .cloned()
            .ok_or_else(|| VaultError::NotFound(format!("identity {}", id.0)))?;
        self.doc.identities.retain(|i| i.id != *id);
        self.drop_secret_if_unreferenced(&record.private_key_secret_id);
        if let Some(p) = &record.passphrase_secret_id {
            self.drop_secret_if_unreferenced(p);
        }
        self.record_deletion(&id.0, DeletionKind::Identity, now_ms());
        self.dirty = true;
        Ok(())
    }

    /// 主机改口令：写入新密文并挂到主机上，旧的若无人引用即回收。
    pub fn set_host_password(&mut self, host_id: &HostId, password: &[u8]) -> Result<(), VaultError> {
        let new_secret = self.put_secret(SecretKind::Password, password)?;
        let old = {
            let host = self
                .doc
                .hosts
                .iter_mut()
                .find(|h| h.id == *host_id)
                .ok_or_else(|| VaultError::NotFound(format!("host {}", host_id.0)))?;
            std::mem::replace(&mut host.password_secret_id, Some(new_secret))
        };
        if let Some(old) = old {
            self.drop_secret_if_unreferenced(&old);
        }
        self.dirty = true;
        Ok(())
    }

    pub fn password_of(&self, host_id: &HostId) -> Result<Option<SecretId>, VaultError> {
        Ok(self.host(host_id).and_then(|h| h.password_secret_id.clone()))
    }

    /// 解出秘密交给回调；回调返回后明文立即清零。
    /// 秘密永远不进 `String`、不进日志、不进 `Clone` 出去的结构。
    pub fn with_secret<T>(
        &self,
        secret_id: &SecretId,
        f: impl FnOnce(&[u8]) -> Result<T, VaultError>,
    ) -> Result<T, VaultError> {
        let entry = self
            .doc
            .secrets
            .get(&secret_id.0)
            .ok_or_else(|| VaultError::NotFound(format!("secret {}", secret_id.0)))?;
        let sealed = base64_decode(&entry.cipher)
            .ok_or_else(|| VaultError::Malformed("secret cipher is not base64".into()))?;
        let plain: Zeroizing<Vec<u8>> = open(&self.key, &sealed)?;
        f(&plain)
    }

    /// 按引用检查删除。还有记录引用时返回 `InUse`，避免误删导致别的主机登不上。
    pub fn remove_secret(&mut self, secret_id: &SecretId) -> Result<(), VaultError> {
        let refs = self.secret_references(secret_id);
        if refs > 0 {
            return Err(VaultError::InUse {
                what: format!("secret {}", secret_id.0),
                count: refs,
            });
        }
        self.doc
            .secrets
            .remove(&secret_id.0)
            .map(|_| ())
            .ok_or_else(|| VaultError::NotFound(format!("secret {}", secret_id.0)))?;
        self.dirty = true;
        Ok(())
    }

    pub fn secret_references(&self, secret_id: &SecretId) -> usize {
        let mut n = 0usize;
        for h in &self.doc.hosts {
            if h.password_secret_id.as_ref() == Some(secret_id) {
                n += 1;
            }
        }
        for i in &self.doc.identities {
            if i.private_key_secret_id == *secret_id {
                n += 1;
            }
            if i.passphrase_secret_id.as_ref() == Some(secret_id) {
                n += 1;
            }
        }
        if self.doc.lock_pin.as_ref() == Some(secret_id) {
            n += 1;
        }
        // 同步串和 PIN 同理：没有任何主机引用它，但它绝不能被回收逻辑当成孤儿
        if self.doc.sync_str.as_ref() == Some(secret_id) {
            n += 1;
        }
        if self.doc.provider_token.as_ref() == Some(secret_id) {
            n += 1;
        }
        n
    }

    fn put_secret(&mut self, kind: SecretKind, plaintext: &[u8]) -> Result<SecretId, VaultError> {
        let sealed = seal(&self.key, plaintext)?;
        let id = SecretId(new_id());
        self.doc.secrets.insert(
            id.0.clone(),
            SecretEntry {
                kind,
                cipher: base64_encode(&sealed),
            },
        );
        Ok(id)
    }

    fn drop_secret_if_unreferenced(&mut self, secret_id: &SecretId) {
        if self.secret_references(secret_id) == 0 {
            self.doc.secrets.remove(&secret_id.0);
        }
    }

    /// 密文条目数（测试与 UI 的「已存凭据 N 条」用，不暴露内容）
    pub fn secret_count(&self) -> usize {
        self.doc.secrets.len()
    }

    /// 这条密文是口令还是私钥。UI 在删除确认时要按这个区分文案。
    pub fn secret_kind(&self, secret_id: &SecretId) -> Option<SecretKind> {
        self.doc.secrets.get(&secret_id.0).map(|e| e.kind)
    }

    /// 是否设过应用锁口令。启动时靠它决定「界面一打开就该是锁着的」还是「直接用」。
    pub fn has_lock_pin(&self) -> bool {
        self.doc.lock_pin.is_some()
    }

    /// 设置或替换解锁口令。空口令由命令层挡掉，这里不重复判：多一处判空就多一处
    /// 「命令层以为拦住了、其实没有」的可能。
    pub fn set_lock_pin(&mut self, pin: &[u8]) -> Result<(), VaultError> {
        let old = self.doc.lock_pin.take();
        let id = self.put_secret(SecretKind::LockPin, pin)?;
        self.doc.lock_pin = Some(id);
        if let Some(old) = old {
            // 旧条目此刻已经没有任何引用，直接回收，不留两把口令
            self.doc.secrets.remove(&old.0);
        }
        self.dirty = true;
        Ok(())
    }

    /// 取消应用锁。没设过也是成功——「清除一个不存在的东西」不该报错。
    pub fn clear_lock_pin(&mut self) {
        if let Some(old) = self.doc.lock_pin.take() {
            self.doc.secrets.remove(&old.0);
            self.dirty = true;
        }
    }

    /// 校验解锁口令，返回是否匹配。
    ///
    /// 明文只在这条调用里出现，比较走完全部字节：提前返回的话，攻击者可以靠响应
    /// 时间差逐位猜出口令，而锁防的正是有本地访问的人。
    pub fn check_lock_pin(&self, pin: &[u8]) -> Result<bool, VaultError> {
        let Some(id) = self.doc.lock_pin.as_ref() else {
            return Ok(false);
        };
        self.with_secret(id, |stored| Ok(constant_time_eq(stored, pin)))
    }

    // ---- 云同步 ----

    /// 这台设备存过的同步串（归一化后的那一版）。明文只在回调里存在。
    pub fn with_sync_str<T>(&self, f: impl FnOnce(&[u8]) -> Result<T, VaultError>) -> Result<T, VaultError> {
        let id = self
            .doc
            .sync_str
            .clone()
            .ok_or_else(|| VaultError::NotFound("sync string".into()))?;
        self.with_secret(&id, f)
    }

    pub fn has_sync_str(&self) -> bool {
        self.doc.sync_str.is_some()
    }

    /// 存下同步串。**传入的必须是 `envelope::normalize` 之后的形式**：
    /// 粘贴带进来的 CRLF 只在录入这一次能看出来，之后它变成「密钥错误」就查不动了。
    ///
    /// 换串必须连基线一起丢：基线是从老那把锁的底子转封来的，新串对不上它，
    /// 留着等于让下一次合并拿一份认不出的数据定胜负。
    pub fn set_sync_str(&mut self, sync_str: &[u8]) -> Result<(), VaultError> {
        let old = self.doc.sync_str.take();
        let id = self.put_secret(SecretKind::SyncStr, sync_str)?;
        self.doc.sync_str = Some(id);
        if let Some(old) = old {
            self.doc.secrets.remove(&old.0);
        }
        self.doc.sync_base = None;
        self.dirty = true;
        Ok(())
    }

    /// 忘记这台设备上的同步串。不动云端那份数据，也不动基线之外的任何东西。
    pub fn clear_sync_str(&mut self) {
        if let Some(old) = self.doc.sync_str.take() {
            self.doc.secrets.remove(&old.0);
            self.doc.sync_base = None;
            self.dirty = true;
        }
    }

    /// 这台设备配过云端令牌吗。UI 只显示「已配置」，永远拿不到值 ——
    /// 令牌出库只发生在 `with_provider_token` 的回调里，而那个回调不出进程。
    pub fn has_provider_token(&self) -> bool {
        self.doc.provider_token.is_some()
    }

    pub fn with_provider_token<T>(
        &self,
        f: impl FnOnce(&[u8]) -> Result<T, VaultError>,
    ) -> Result<T, VaultError> {
        let id = self
            .doc
            .provider_token
            .clone()
            .ok_or_else(|| VaultError::NotFound("provider token".into()))?;
        self.with_secret(&id, f)
    }

    pub fn set_provider_token(&mut self, token: &[u8]) -> Result<(), VaultError> {
        let old = self.doc.provider_token.take();
        let id = self.put_secret(SecretKind::ProviderToken, token)?;
        self.doc.provider_token = Some(id);
        if let Some(old) = old {
            self.doc.secrets.remove(&old.0);
        }
        self.dirty = true;
        Ok(())
    }

    pub fn clear_provider_token(&mut self) {
        if let Some(old) = self.doc.provider_token.take() {
            self.doc.secrets.remove(&old.0);
            self.dirty = true;
        }
    }

    /// 云端那一份的坐标。`None` = 还没在上面建过片段。
    pub fn sync_target(&self) -> Option<&SyncTarget> {
        self.doc.sync_target.as_ref()
    }

    pub fn set_sync_target(&mut self, target: Option<SyncTarget>) {
        if self.doc.sync_target != target {
            self.doc.sync_target = target;
            self.dirty = true;
        }
    }

    /// 上一次同步的基线快照。解不开时返回 `Err`：调用方可以按「没有基线」继续合并
    /// （结论会变保守，不会静默丢数据），但不能当成「本来就没有」直接盖过去。
    pub fn sync_base(&self) -> Result<Option<SyncSnapshot>, VaultError> {
        let Some(text) = self.doc.sync_base.as_deref() else {
            return Ok(None);
        };
        let sealed = base64_decode(text)
            .ok_or_else(|| VaultError::Malformed("sync baseline is not base64".into()))?;
        let plain = open(&self.key, &sealed)?;
        let snap: SyncSnapshot = serde_json::from_slice(&plain)
            .map_err(|e| VaultError::Malformed(format!("sync baseline: {e}")))?;
        snap
            .check_version()
            .map_err(VaultError::Malformed)?;
        Ok(Some(snap))
    }

    /// 记下「这次同步完成时本地是哪一份」。`None` 清空基线（换云端目标时用）。
    pub fn set_sync_base(&mut self, snap: Option<&SyncSnapshot>) -> Result<(), VaultError> {
        let next = match snap {
            None => None,
            Some(s) => {
                let json = serde_json::to_vec(s)
                    .map_err(|e| VaultError::Malformed(format!("baseline json: {e}")))?;
                Some(base64_encode(&seal(&self.key, &json)?))
            }
        };
        if self.doc.sync_base != next {
            self.doc.sync_base = next;
            self.dirty = true;
        }
        Ok(())
    }

    /// 本机已有的密文条目 id，合并时用来判断「远端这条要不要导入」。
    pub fn secret_ids(&self) -> Vec<String> {
        self.doc.secrets.keys().cloned().collect()
    }

    /// 导出一份可上传的快照。`include_private_keys` 就是设置里那个默认不勾的开关。
    ///
    /// 纳入哪些密文**只由「被留下的记录引用了什么」决定**：先定记录和身份，
    /// 再顺着它们的 id 字段把密文收齐。反过来按 `auth_mode` 猜会造出悬空引用——
    /// 身份是全局的，一台主机完全可以指向一个本机没同步过来的身份。
    /// 主机记录则一律带上：这样两台设备对同一条主机的字节一致，
    /// 不会把「B 手上没这把钥匙」误判成「B 改过这条主机」。
    pub fn export_snapshot(
        &self,
        key: &DerivedKey,
        include_private_keys: bool,
    ) -> Result<SyncSnapshot, VaultError> {
        self.collect_snapshot(include_private_keys, |plain| {
            Ok(base64_encode(&crate::crypto::seal(key.inner(), plain)?))
        })
    }

    /// 与 `export_snapshot` 同一份内容，但密文用**本机主密钥**封。
    ///
    /// 用途只有两个：写基线，以及判断「这次到底有没有东西要推」。
    /// 这份绝不能上传——它的锁只有本机解得开，第二台设备拿到就是废数据。
    pub fn local_snapshot(&self, include_private_keys: bool) -> Result<SyncSnapshot, VaultError> {
        self.collect_snapshot(include_private_keys, |plain| {
            Ok(base64_encode(&seal(&self.key, plain)?))
        })
    }

    /// 两条导出路径共用的收集逻辑。「哪些记录、哪些密文进快照」这件事只允许有一处实现：
    /// 基线如果是按另一套规则收集的，`same_content` 就会永远判定「本地变过了」。
    fn collect_snapshot(
        &self,
        include_private_keys: bool,
        reseal: impl Fn(&[u8]) -> Result<String, VaultError>,
    ) -> Result<SyncSnapshot, VaultError> {
        let mut snap = SyncSnapshot::new(self.doc.device_id.clone(), now_ms());
        snap.hosts = self.doc.hosts.clone();
        if include_private_keys {
            snap.identities = self.doc.identities.clone();
        }
        let mut wanted: Vec<SecretId> = Vec::new();
        for h in &snap.hosts {
            wanted.extend(h.password_secret_id.iter().cloned());
        }
        for i in &snap.identities {
            wanted.push(i.private_key_secret_id.clone());
            wanted.extend(i.passphrase_secret_id.iter().cloned());
        }
        for id in wanted {
            if snap.secrets.iter().any(|(k, _)| *k == id.0) {
                continue; // 两条主机共用一条口令密文是允许的
            }
            let kind = self
                .doc
                .secrets
                .get(&id.0)
                .ok_or_else(|| VaultError::NotFound(format!("secret {}", id.0)))?
                .kind;
            // 本机主密钥解开 → 换那把锁重封，明文只活在这一对括号里
            let cipher = self.with_secret(&id, |plain| reseal(plain))?;
            snap.secrets.push((id.0.clone(), SnapshotSecret { kind, cipher }));
        }
        snap.tombstones = self.doc.tombstones.clone();
        Ok(snap)
    }

    /// 执行一份合并计划，把本地文档改成合并结论要求的样子。
    ///
    /// 顺序是定死的：密文先入库，紧接着写记录才不会引用到不存在的条目；
    /// 删除标记最后并，避免中途被记录写入的「这条又活了」清理逻辑误吃掉。
    /// 某条密文解不开就返回 `Err` 且不再碰后面的记录；已经入库的那几条只是没人引用的
    /// 多余密文，不构成安全问题，下一次成功的合并末尾会把它们收掉。
    pub fn apply_plan(&mut self, key: &DerivedKey, plan: &MergePlan) -> Result<(), VaultError> {
        for (id, secret) in &plan.secrets_import {
            let sealed = base64_decode(&secret.cipher)
                .ok_or_else(|| VaultError::Malformed("sync secret cipher is not base64".into()))?;
            let plain = crate::crypto::open(key.inner(), &sealed)?;
            self.insert_secret_at(id, secret.kind, &plain)?;
        }
        for id in &plan.identities_remove {
            self.doc.identities.retain(|i| i.id != *id);
        }
        for record in &plan.identities_upsert {
            self.put_identity(record);
        }
        for id in &plan.hosts_remove {
            self.doc.hosts.retain(|h| h.id != *id);
        }
        for record in &plan.hosts_upsert {
            self.put_host(record);
        }
        for mark in &plan.tombstones {
            self.merge_tombstone(mark);
        }
        self.gc_secrets();
        self.dirty = true;
        Ok(())
    }

    /// 合并用：按 id 覆盖一条主机，**不改它的时间戳**。
    ///
    /// 不能走 `upsert_host`：它把 `updated_at_ms` 刷成现在，合并进来的记录一旦被刷，
    /// 下一次同步就会把这条当成「本机刚改过」，胜负手当场倒转。
    fn put_host(&mut self, record: &HostRecord) {
        match self.doc.hosts.iter_mut().find(|h| h.id == record.id) {
            Some(existing) => *existing = record.clone(),
            None => self.doc.hosts.push(record.clone()),
        }
        self.doc
            .tombstones
            .retain(|t| !(t.kind == DeletionKind::Host && t.id == record.id.0));
    }

    /// 同 `put_host`，身份一类。副本的 id 是新生成的，所以合并里几乎总是走「追加」那支。
    fn put_identity(&mut self, record: &IdentityRecord) {
        match self.doc.identities.iter_mut().find(|i| i.id == record.id) {
            Some(existing) => *existing = record.clone(),
            None => self.doc.identities.push(record.clone()),
        }
        self.doc
            .tombstones
            .retain(|t| !(t.kind == DeletionKind::Identity && t.id == record.id.0));
    }

    /// 插入一条从云端解开的密文，保留它**原本的 id**。
    ///
    /// 同 id 已存在就一个字节都不动。整条链路的正确性建立在「同 id 即同内容」这条
    /// 不变量上，而覆盖一条本机正在引用的密文，恰恰是它会崩的样子。
    fn insert_secret_at(&mut self, id: &str, kind: SecretKind, plain: &[u8]) -> Result<(), VaultError> {
        if self.doc.secrets.contains_key(id) {
            return Ok(());
        }
        let sealed = seal(&self.key, plain)?;
        self.doc
            .secrets
            .insert(id.to_string(), SecretEntry { kind, cipher: base64_encode(&sealed) });
        Ok(())
    }

    /// 回收不再被任何记录引用的密文。合并后调用：删掉一侧的记录必然留下孤儿。
    fn gc_secrets(&mut self) {
        let orphans: Vec<String> = self
            .doc
            .secrets
            .keys()
            .filter(|id| self.secret_references(&SecretId((*id).clone())) == 0)
            .cloned()
            .collect();
        for id in orphans {
            self.doc.secrets.remove(&id);
        }
    }
}

/// 长度保密的等长比较：先看长度，再把每个字节的差异累积到最后。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 新生成一个记录 id。`snapshot::plan_merge` 给冲突副本换 id 时用它——
/// id 的形状属于 vault，不属于那块纯逻辑。
pub fn new_id() -> String {
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    // 顺手做成 RFC 4122 形状的 v4 变体，将来与别的系统对 id 时少一层翻译
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(32);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub(crate) use dbhx_model::now_ms;

/// 不引 base64 crate：这里只需要「字节 <-> 标准 base64 无换行」，
/// 手写 40 行可以少一个依赖，也让交叉编译矩阵的依赖树更短。
/// `pub(crate)` 是给 `envelope` 生成同步串用的（24 字节 → base64url 正好 32 字符）。
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        // padding 只能出现在第 3、4 位：'=' 出现在前两位（"A=Bc"）一律非法
        let pad = match (chunk[2], chunk[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            (b'=', _) => return None,
            _ => 0,
        };
        if chunk[..4 - pad].contains(&b'=') {
            return None;
        }
        // '=' 也参与 6 位移位（值当 0），最后按 pad 丢弃尾部字节；
        // 早先版本在这里 `continue` 跳过了移位，长度不是 3 的倍数的输入会解错。
        let mut n = 0u32;
        for c in chunk {
            let v = if *c == b'=' { 0 } else { val(*c)? as u32 };
            n = n << 6 | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{plan_merge, MergeInput, MergePolicy};
    use dbhx_model::{AuthMode, Endpoint, OsType, Protocol, TerminalEncoding};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbhx-vault-{tag}-{}-{}",
            std::process::id(),
            new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

    fn memory_vault(dir: &Path) -> Vault {
        Vault::open(dir, KeySource::Memory(MasterKey::generate())).unwrap()
    }

    #[test]
    fn secrets_survive_a_reopen_but_plaintext_is_never_in_the_file() {
        let dir = temp_dir("roundtrip");
        let key_path = dir.join("master.key");
        let mut v = Vault::open(&dir, KeySource::File(key_path.clone())).unwrap();
        let id = v.upsert_host(host("db1"));
        v.set_host_password(&id, "s3cr3t-口令".as_bytes()).unwrap();
        v.flush().unwrap();
        drop(v);

        let on_disk = std::fs::read(dir.join("vault.json")).unwrap();
        assert!(!on_disk.windows(6).any(|w| w == b"s3cr3t"), "明文进了落盘文件");

        let reopened = Vault::open(&dir, KeySource::File(key_path)).unwrap();
        let secret_id = reopened
            .host(&id)
            .unwrap()
            .password_secret_id
            .clone()
            .unwrap();
        let got = reopened.with_secret(&secret_id, |p| Ok(p.to_vec())).unwrap();
        assert_eq!(got, "s3cr3t-口令".as_bytes());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_master_key_yields_crypto_error_not_a_panic() {
        let dir = temp_dir("wrongkey");
        let mut v = memory_vault(&dir);
        let id = v.upsert_host(host("db1"));
        v.set_host_password(&id, b"pw").unwrap();
        v.flush().unwrap();
        drop(v);

        let again = Vault::open(&dir, KeySource::Memory(MasterKey::generate()));
        assert!(matches!(again, Err(VaultError::Crypto)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn password_rotation_collects_the_orphaned_secret() {
        let mut v = memory_vault(&temp_dir("rotate"));
        let id = v.upsert_host(host("db1"));
        v.set_host_password(&id, b"old").unwrap();
        assert_eq!(v.secret_count(), 1);
        v.set_host_password(&id, b"new").unwrap();
        assert_eq!(v.secret_count(), 1, "旧口令无人引用却没被回收");
    }

    #[test]
    fn shared_secret_cannot_be_deleted_while_referenced() {
        let mut v = memory_vault(&temp_dir("inuse"));
        let a = v.upsert_host(host("a"));
        let b = v.upsert_host(host("b"));
        v.set_host_password(&a, b"pw").unwrap();
        let secret = v.host(&a).unwrap().password_secret_id.clone().unwrap();
        // 手工让第二台主机共用同一条密文（模拟导入场景）
        v.doc.hosts.iter_mut().find(|h| h.id == b).unwrap().password_secret_id = Some(secret.clone());
        assert_eq!(v.secret_references(&secret), 2);
        assert!(matches!(v.remove_secret(&secret), Err(VaultError::InUse { count: 2, .. })));
        v.remove_host(&a).unwrap();
        v.remove_secret(&secret).unwrap_err(); // 还剩 b 引用
        v.remove_host(&b).unwrap();
        assert_eq!(v.secret_count(), 0, "删完主机后密文应被级联回收");
    }

    #[test]
    fn identity_stores_key_and_passphrase_as_two_secrets() {
        let mut v = memory_vault(&temp_dir("identity"));
        let id = v
            .add_identity("work", b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n", Some(b"pp"))
            .unwrap();
        assert_eq!(v.secret_count(), 2);
        let rec = v.identities().iter().find(|i| i.id == id).unwrap();
        assert_eq!(
            v.secret_kind(&rec.private_key_secret_id),
            Some(SecretKind::PrivateKey)
        );
        assert_eq!(
            v.secret_kind(rec.passphrase_secret_id.as_ref().unwrap()),
            Some(SecretKind::Passphrase)
        );
        let key = v
            .with_secret(&rec.private_key_secret_id, |p| Ok(p.to_vec()))
            .unwrap();
        assert!(key.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----"));
        let pp = v
            .with_secret(rec.passphrase_secret_id.as_ref().unwrap(), |p| Ok(p.to_vec()))
            .unwrap();
        assert_eq!(pp, b"pp");
        v.remove_identity(&id).unwrap();
        assert_eq!(v.secret_count(), 0);
    }

    #[test]
    fn empty_passphrase_is_not_stored_as_a_secret() {
        let mut v = memory_vault(&temp_dir("empty-pp"));
        v.add_identity("agent-less", b"KEY", Some(b"")).unwrap();
        assert_eq!(v.secret_count(), 1);
    }

    #[test]
    fn missing_host_removal_is_not_found() {
        let mut v = memory_vault(&temp_dir("missing"));
        let err = v.remove_host(&HostId("nope".into())).unwrap_err();
        assert!(matches!(err, VaultError::NotFound(_)));
    }

    #[test]
    fn corrupt_blob_is_reported_as_malformed_and_file_is_left_intact() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("vault.json"), b"this is not base64 at all!!").unwrap();
        let err = Vault::open(&dir, KeySource::Memory(MasterKey::generate())).unwrap_err();
        assert!(matches!(err, VaultError::Malformed(_)), "{err:?}");
        assert!(dir.join("vault.json").exists(), "打开失败不该覆盖原文件");
    }

    #[test]
    fn base64_matches_std_encoding_including_padding_cases() {
        for len in 0..17usize {
            let bytes: Vec<u8> = (0..len as u8).collect();
            let enc = base64_encode(&bytes);
            assert_eq!(enc.len(), (len + 2) / 3 * 4);
            assert_eq!(base64_decode(&enc).unwrap(), bytes, "len={len} enc={enc}");
        }
        assert!(base64_decode("QQ=").is_none(), "padding 位置非法必须拒绝");
        assert!(base64_decode("Q!==").is_none());
    }

    // ---- 云同步：导出、重密封、执行合并计划 ----

    /// 一次派生都不做：`for_test` 给的是现成的密钥，
    /// 「这把密钥怎么来的」由 `envelope` 和下面那条端到端用例负责。
    fn sync_vault(tag: &str) -> (Vault, MasterKey, PathBuf) {
        let dir = temp_dir(tag);
        let local = MasterKey::generate();
        let vault = Vault::open(&dir, KeySource::Memory(local.clone())).unwrap();
        (vault, local, dir)
    }

    fn derived(key: MasterKey) -> DerivedKey {
        DerivedKey::for_test(key)
    }

    /// 把一份快照里某条密文用「同步密钥」解出来，验证云端那份确实只认这把锁。
    fn open_with_sync(sync: &MasterKey, cipher: &str) -> Vec<u8> {
        let sealed = base64_decode(cipher).expect("导出的密文应当是 base64");
        open(sync, &sealed).unwrap().to_vec()
    }

    /// 模拟「云端送来的一条密文」：用同步密钥封一份明文，转成快照里那个 base64。
    fn cloud_cipher(sync: &MasterKey, plain: &[u8]) -> String {
        base64_encode(&seal(sync, plain).unwrap())
    }

    /// 跑一次「远端为唯一一方、没有共同基线」的合并并把计划执行掉。
    fn merge_into(v: &mut Vault, remote: &SyncSnapshot, key: &DerivedKey) -> MergePlan {
        merge_from(v, remote, None, key)
    }

    fn merge_from(
        v: &mut Vault,
        remote: &SyncSnapshot,
        base: Option<&SyncSnapshot>,
        key: &DerivedKey,
    ) -> MergePlan {
        let ids = v.secret_ids();
        let plan = plan_merge(
            &MergeInput {
                local_hosts: v.hosts(),
                local_identities: v.identities(),
                local_tombstones: v.tombstones(),
                local_device_id: v.device_id(),
                local_secret_ids: &ids,
                remote,
                base,
                policy: MergePolicy::ThreeWay,
            },
            new_id,
        );
        v.apply_plan(key, &plan).unwrap();
        plan
    }

    #[test]
    fn the_private_key_switch_decides_whether_the_identity_and_its_secrets_leave() {
        let (mut v, local, dir) = sync_vault("switch");
        let sync = MasterKey::generate();
        let id = v
            .add_identity("work", b"KEY-BYTES", Some(b"passphrase"))
            .unwrap();
        let mut h = host("用钥匙的那台");
        h.auth_mode = AuthMode::PublicKey;
        h.identity_id = Some(id.clone());
        v.upsert_host(h);

        let off = v.export_snapshot(&derived(sync.clone()), false).unwrap();
        assert!(off.identities.is_empty(), "开关没勾时身份不能出库");
        assert_eq!(off.secrets.len(), 0, "没勾时连钥匙和口令密文都不该带上");
        assert_eq!(off.hosts.len(), 1, "主机记录一律要带，否则两台设备的字节不一致");
        assert_eq!(off.hosts[0].identity_id, Some(id.clone()), "引用照旧留着，由导入侧报缺身份");

        let on = v.export_snapshot(&derived(sync.clone()), true).unwrap();
        assert_eq!(on.identities.len(), 1);
        assert_eq!(
            on.secrets.len(),
            2,
            "钥匙与 passphrase 是两条独立密文，勾选项要同时决定这两条去不去"
        );
        assert_eq!(on.hosts, off.hosts, "开关只动身份那一侧，主机字节必须一致");
        let rec = v.identities().iter().find(|i| i.id == id).unwrap().clone();
        for (sid, secret) in &on.secrets {
            let plain = open_with_sync(&sync, &secret.cipher);
            let expected = if *sid == rec.private_key_secret_id.0 {
                b"KEY-BYTES".to_vec()
            } else {
                b"passphrase".to_vec()
            };
            assert_eq!(plain, expected, "重密封后按 id 回来的内容对不上");
            // 云端那份绝不能被本机的 master.key 解开——它换了一把锁
            let bytes = base64_decode(&secret.cipher).unwrap();
            assert!(
                open(&local, &bytes).is_err(),
                "本机主密钥能解开云端条目，说明根本没换同步密钥"
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn neither_the_lock_pin_sync_string_nor_the_token_ever_leaves_the_device() {
        let (mut v, _local, dir) = sync_vault("never-leaves");
        let hid = v.upsert_host(host("db1"));
        v.set_host_password(&hid, b"pw").unwrap();
        v.set_lock_pin(b"1234").unwrap();
        v.set_sync_str(b"0123456789abcdefghijKLMNOPqrst").unwrap();
        v.set_provider_token(b"ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        // 四条密文都在库里，导出只能带走口令那一条。
        // 云端令牌要是跟着快照上了云，就等于把「谁能改这个片段」交给了「谁能读这个片段」。
        assert_eq!(v.secret_count(), 4);
        let snap = v.export_snapshot(&derived(MasterKey::generate()), true).unwrap();
        assert_eq!(snap.secrets.len(), 1, "只有被记录引用到的密文才参与导出");
        assert_eq!(
            snap.secrets[0].0,
            v.host(&hid).unwrap().password_secret_id.as_ref().unwrap().0
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_shared_credential_is_exported_once_not_twice() {
        let (mut v, _local, dir) = sync_vault("shared");
        let a = v.upsert_host(host("a"));
        let b = v.upsert_host(host("b"));
        v.set_host_password(&a, "同一条口令".as_bytes()).unwrap();
        let shared = v.host(&a).unwrap().password_secret_id.clone().unwrap();
        v.doc
            .hosts
            .iter_mut()
            .find(|h| h.id == b)
            .unwrap()
            .password_secret_id = Some(shared.clone());
        let snap = v.export_snapshot(&derived(MasterKey::generate()), false).unwrap();
        assert_eq!(snap.secrets.len(), 1, "两台主机共用一条密文，导出不能变成两条");
        assert_eq!(snap.secrets[0].0, shared.0);
        std::fs::remove_dir_all(dir).ok();
    }

    /// 走完 `export → 封 → 解 → 合并 → 落地` 整条真实链路，含两次真派生。
    /// 这是唯一一条不靠 `for_test` 的用例：换设备这件事只能这样自证一次。
    #[test]
    fn a_push_and_a_later_pull_really_round_trip_through_the_kdf() {
        let sync_str = "0123456789abcdefghijKLMNOPqrst";
        let (mut v, _local, dir_a) = sync_vault("loop-a");
        let hid = v.upsert_host(host("生产库"));
        v.set_host_password(&hid, "云端那条口令".as_bytes()).unwrap();

        let push = DerivedKey::for_push(sync_str).unwrap();
        let blob = push
            .seal_snapshot(&v.export_snapshot(&push, false).unwrap())
            .unwrap();

        let (mut other, _local_b, dir_b) = sync_vault("loop-b");
        let (pull, remote) = DerivedKey::from_blob(sync_str, &blob).unwrap();
        merge_into(&mut other, &remote, &pull);
        let sid = other
            .host(&hid)
            .and_then(|h| h.password_secret_id.clone())
            .expect("同步过来的主机该带着它的口令引用");
        assert_eq!(
            other.with_secret(&sid, |p| Ok(p.to_vec())).unwrap(),
            "云端那条口令".as_bytes()
        );
        std::fs::remove_dir_all(dir_a).ok();
        std::fs::remove_dir_all(dir_b).ok();
    }

    #[test]
    fn import_lands_the_remote_row_with_the_remote_timestamp() {
        let (mut v, _local, dir) = sync_vault("apply");
        let sync = MasterKey::generate();
        let mut h = host("云端来的那台");
        h.id = HostId("h-remote".into());
        h.updated_at_ms = 777;
        h.password_secret_id = Some(SecretId("s-remote".into()));
        let mut remote = SyncSnapshot::new("BBB".into(), 1_000);
        remote.secrets.push((
            "s-remote".into(),
            SnapshotSecret {
                kind: SecretKind::Password,
                cipher: cloud_cipher(&sync, b"cloud-pw"),
            },
        ));
        remote.hosts.push(h);
        let plan = merge_into(&mut v, &remote, &derived(sync));
        assert_eq!(plan.secrets_import.len(), 1);
        let got = v.host(&HostId("h-remote".into())).unwrap();
        assert_eq!(got.updated_at_ms, 777, "合并写入绝不能把时间戳刷成现在");
        let sid = got.password_secret_id.clone().unwrap();
        assert_eq!(
            v.with_secret(&sid, |p| Ok(p.to_vec())).unwrap(),
            b"cloud-pw",
            "导入的口令要用本机主密钥重新封好才能落地"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_secret_id_already_local_is_never_overwritten() {
        // 「同 id 即同内容」这条不变量一旦被破坏，覆盖本地条目就会让别的主机登不上；
        // 所以导入侧遇到同 id 一律不动，哪怕云端那份更新。
        let (mut v, _local, dir) = sync_vault("no-clobber");
        let hid = v.upsert_host(host("db1"));
        v.set_host_password(&hid, "本地这份是对的".as_bytes()).unwrap();
        let sid = v.host(&hid).unwrap().password_secret_id.clone().unwrap();
        let sync = MasterKey::generate();
        let plan = MergePlan {
            secrets_import: vec![(
                sid.0.clone(),
                SnapshotSecret {
                    kind: SecretKind::Password,
                    cipher: cloud_cipher(&sync, "云端想改写".as_bytes()),
                },
            )],
            ..Default::default()
        };
        v.apply_plan(&derived(sync), &plan).unwrap();
        assert_eq!(
            v.with_secret(&sid, |p| Ok(p.to_vec())).unwrap(),
            "本地这份是对的".as_bytes()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_deleted_record_leaves_no_orphan_credential_behind() {
        let (mut v, _local, dir) = sync_vault("gc");
        let hid = v.upsert_host(host("要没的那台"));
        v.set_host_password(&hid, b"pw").unwrap();
        assert_eq!(v.secret_count(), 1);
        let mut remote = SyncSnapshot::new("BBB".into(), 1_000);
        remote.tombstones.push(DeletionMark {
            id: hid.0.clone(),
            kind: DeletionKind::Host,
            deleted_at_ms: 500,
            device_id: "BBB".into(),
        });
        // 基线里就是本地这一条 ⇒ 本机压根没动过它，删除胜出时不必留副本
        let mut base = SyncSnapshot::new("AAA".into(), 900);
        base.hosts.push(v.host(&hid).unwrap().clone());
        merge_from(&mut v, &remote, Some(&base), &derived(MasterKey::generate()));
        assert!(v.host(&hid).is_none());
        assert_eq!(v.secret_count(), 0, "删掉的记录必须连带回收只被它引用的密文");
        assert_eq!(v.tombstones().len(), 1, "删除标记要留在本地，防它下次复活");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 没有基线时「这条到底是别人删的、还是我还没建」无从分辨，
    /// 所以删除照样胜出、但内容一律留成冲突副本 —— 宁可罗嗦，不静默丢数据。
    #[test]
    fn a_deletion_without_a_baseline_deletes_but_still_saves_a_copy() {
        let (mut v, _local, dir) = sync_vault("gc-no-base");
        let hid = v.upsert_host(host("云端先删掉的那台"));
        v.set_host_password(&hid, b"pw").unwrap();
        let mut remote = SyncSnapshot::new("BBB".into(), 1_000);
        remote.tombstones.push(DeletionMark {
            id: hid.0.clone(),
            kind: DeletionKind::Host,
            deleted_at_ms: 500,
            device_id: "BBB".into(),
        });
        let plan = merge_into(&mut v, &remote, &derived(MasterKey::generate()));
        assert_eq!(plan.conflicts.len(), 1, "没基线就留一份副本");
        assert!(v.host(&hid).is_none(), "原条目仍然按删除胜出");
        assert_eq!(v.hosts().len(), 1);
        assert_eq!(v.secret_count(), 1, "副本还引用着那条口令，回收不能把它带走");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn undecryptable_import_fails_loudly_instead_of_importing_garbage() {
        let (mut v, _local, dir) = sync_vault("bad-import");
        let sync = MasterKey::generate();
        let one = |id: &str, cipher: String| MergePlan {
            secrets_import: vec![(
                id.into(),
                SnapshotSecret {
                    kind: SecretKind::Password,
                    cipher,
                },
            )],
            ..Default::default()
        };
        // 不是 base64：格式问题，归 Malformed
        let key = derived(sync.clone());
        assert!(matches!(
            v.apply_plan(&key, &one("s-not-base64", "这不是 base64!!".into())),
            Err(VaultError::Malformed(_))
        ));
        // 是 base64 但用别的同步密钥封的：解不开，归 Crypto（也就是「串不对」那一类）
        let other = cloud_cipher(&MasterKey::generate(), b"x");
        assert!(matches!(
            v.apply_plan(&key, &one("s-other-key", other)),
            Err(VaultError::Crypto)
        ));
        assert_eq!(v.secret_count(), 0, "解不开的条目不该留在库里");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sync_string_token_and_baseline_survive_gc_and_a_reopen() {
        let dir = temp_dir("sync-state");
        let key_path = dir.join("master.key");
        let mut v = Vault::open(&dir, KeySource::File(key_path.clone())).unwrap();
        v.set_sync_str(b"0123456789abcdefghijKLMNOPqrst").unwrap();
        v.set_provider_token(b"ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        v.set_sync_target(Some(SyncTarget { provider: dbhx_model::ProviderKind::GitHub, id: "deadbeef".into() }));
        let hid = v.upsert_host(host("临时的一台"));
        v.set_host_password(&hid, b"pw").unwrap();
        // 基线要能用本机密钥原样读回来，否则每次同步都等于没有基线
        let base = v.local_snapshot(false).unwrap();
        v.set_sync_base(Some(&base)).unwrap();
        // 一次会大规模回收孤儿密文的合并，不能把这几条「没人引用」的条目一起清掉
        let mut remote = SyncSnapshot::new("BBB".into(), 1_000);
        remote.tombstones.push(DeletionMark {
            id: hid.0.clone(),
            kind: DeletionKind::Host,
            deleted_at_ms: 500,
            device_id: "BBB".into(),
        });
        merge_from(
            &mut v,
            &remote,
            Some(&base),
            &derived(MasterKey::generate()),
        );
        assert_eq!(
            v.secret_count(),
            2,
            "口令条目该被回收，同步串和令牌这两条「没人引用」的得留着"
        );
        assert_eq!(v.sync_base().unwrap().unwrap().hosts.len(), 1, "基线里的这台主机不能跟着消失");
        v.flush().unwrap();
        drop(v);

        let reopened = Vault::open(&dir, KeySource::File(key_path)).unwrap();
        assert!(reopened.has_sync_str());
        assert!(reopened.has_provider_token());
        assert_eq!(reopened.sync_target().unwrap().id, "deadbeef");
        let reopened_base = reopened.sync_base().unwrap().expect("重开之后基线还在");
        assert!(reopened_base.same_content(&base), "基线读回来变了，说明它没被原样存下");
        let revealed = reopened
            .with_sync_str(|p| Ok(String::from_utf8(p.to_vec())))
            .unwrap()
            .unwrap();
        assert_eq!(revealed, "0123456789abcdefghijKLMNOPqrst");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 换同步串等于换了一把锁：老基线是用旧串的同步密钥解出来再转封的，
    /// 留着它只会让下一次合并拿一份对不上号的底子定胜负。
    #[test]
    fn changing_or_forgetting_the_sync_string_always_drops_the_baseline() {
        let (mut v, _key, dir) = sync_vault("base-drop");
        let hid = v.upsert_host(host("一台"));
        v.set_host_password(&hid, b"pw").unwrap();
        let base = v.local_snapshot(false).unwrap();
        v.set_sync_base(Some(&base)).unwrap();
        assert!(v.sync_base().unwrap().is_some());

        v.set_sync_str(b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").unwrap();
        assert!(v.sync_base().unwrap().is_none(), "换了串还留着老基线");

        v.set_sync_base(Some(&base)).unwrap();
        v.clear_sync_str();
        assert!(v.sync_base().unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    /// v1 老库（没有 device_id、没有删除标记、没有同步字段）必须原地升上来，
    /// 而且 `device_id` 只能生成一次——每进程换一个，跨设备定序就成了掷骰子。
    #[test]
    fn a_v1_vault_upgrades_in_place_and_keeps_one_device_id_forever() {
        let dir = temp_dir("migrate");
        let key_path = dir.join("master.key");
        let key = KeySource::File(key_path.clone()).load_or_create().unwrap();
        // 先造一个正常的 v2 库，再把版本号改回 1、抹掉 v2 才有的字段：
        // 记录部分原样留着，比手写一份 v1 JSON 更接近真实老库。
        {
            let mut v = Vault::open(&dir, KeySource::File(key_path.clone())).unwrap();
            let hid = v.upsert_host(host("老库里唯一的一台"));
            v.set_host_password(&hid, "老口令".as_bytes()).unwrap();
            v.flush().unwrap();
        }
        let raw = base64_decode(
            &String::from_utf8(std::fs::read(dir.join("vault.json")).unwrap()).unwrap(),
        )
        .unwrap();
        let mut doc: serde_json::Value = serde_json::from_slice(&open(&key, &raw).unwrap()).unwrap();
        doc["version"] = serde_json::json!(1u32);
        for gone in [
            "device_id",
            "tombstones",
            "sync_str",
            "sync_base",
            "sync_target",
            "provider_token",
        ] {
            doc.as_object_mut().unwrap().remove(gone);
        }
        let sealed = seal(&key, &serde_json::to_vec(&doc).unwrap()).unwrap();
        std::fs::write(dir.join("vault.json"), base64_encode(&sealed).as_bytes()).unwrap();

        let mut v = Vault::open(&dir, KeySource::File(key_path.clone())).unwrap();
        let first_device = v.device_id().to_string();
        assert!(!first_device.is_empty(), "迁移必须补上 device_id");
        assert_eq!(v.hosts().len(), 1, "老记录一条都不能少");
        let sid = v.hosts()[0].password_secret_id.clone().unwrap();
        assert_eq!(
            v.with_secret(&sid, |p| Ok(p.to_vec())).unwrap(),
            "老口令".as_bytes(),
            "升版不该动任何一条密文"
        );
        v.flush().unwrap();

        let raw = base64_decode(
            &String::from_utf8(std::fs::read(dir.join("vault.json")).unwrap()).unwrap(),
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&open(&key, &raw).unwrap()).unwrap();
        assert_eq!(doc["version"], serde_json::json!(DOC_VERSION), "升完版必须落盘");
        assert_eq!(doc["device_id"], serde_json::json!(&first_device));

        let again = Vault::open(&dir, KeySource::File(key_path)).unwrap();
        assert_eq!(again.device_id(), first_device, "device_id 换进程就变了，定序会失效");
        std::fs::remove_dir_all(dir).ok();
    }
}
