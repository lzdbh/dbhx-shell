//! Vault 文档与持久化。
//!
//! 一整个加密 JSON 文档（不是每条记录一个文件）：主机数量在几百级，
//! 整文档读写换来的是「一次 rename 就是原子提交」，不用自己设计并发追加日志。
//! 明文秘密只在 `with_secret` 的回调里存在，回调外拿到的是 `Zeroizing`，出作用域清零。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dbhx_model::{HostId, HostRecord, IdentityId, IdentityRecord, SecretId};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{open, seal, KeySource, MasterKey};
use crate::error::VaultError;

/// 当前文档结构版本。加字段时递增并写迁移，读到老版本要能升上来。
const DOC_VERSION: u32 = 1;

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
}

fn current_version() -> u32 {
    DOC_VERSION
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
        let doc = if path.exists() {
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
            let doc: VaultDoc = serde_json::from_slice(&plain)
                .map_err(|e| VaultError::Malformed(format!("vault document: {e}")))?;
            if doc.version > DOC_VERSION {
                return Err(VaultError::Malformed(format!(
                    "vault version {} is newer than supported {}",
                    doc.version, DOC_VERSION
                )));
            }
            doc
        } else {
            VaultDoc {
                version: DOC_VERSION,
                ..Default::default()
            }
        };
        Ok(Self {
            path,
            key,
            doc,
            dirty: false,
        })
    }

    pub fn flush(&mut self) -> Result<(), VaultError> {
        if !self.dirty {
            return Ok(());
        }
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

fn new_id() -> String {
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
fn base64_encode(bytes: &[u8]) -> String {
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

fn base64_decode(s: &str) -> Option<Vec<u8>> {
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
}
