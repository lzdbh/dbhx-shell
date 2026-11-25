//! AES-256-GCM 密封与主密钥来源。
//!
//! 密文格式（一条记录一个 nonce，不用随机数复用）：
//! `base64( nonce[12] || ciphertext || tag[16] )`。
//! 选 GCM 而不是 CBC：后者没有完整性校验，本地文件被改一字节会解出垃圾而不报错。

use std::path::{Path, PathBuf};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::RngCore;
use zeroize::Zeroizing;

use crate::error::VaultError;

const NONCE_LEN: usize = 12;

/// vault 主密钥。`Zeroizing` 保证离开作用域即清零，不落 swap 之外的地方；
/// Clone 会多出一份副本，所以只在必须复制时显式 `.clone()`。
#[derive(Clone)]
pub struct MasterKey(Zeroizing<Vec<u8>>);

impl MasterKey {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, VaultError> {
        if bytes.len() != 32 {
            return Err(VaultError::Malformed(format!(
                "master key must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self(Zeroizing::new(bytes)))
    }

    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let mut buf = vec![0u8; 32];
        rng.fill_bytes(&mut buf);
        Self(Zeroizing::new(buf))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 32 字节密钥的 Debug 必须是红acted；见 dbhx-model::redacted 的约定
        f.debug_struct("MasterKey").finish_non_exhaustive()
    }
}

pub fn seal(key: &MasterKey, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|_| VaultError::Crypto)?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| VaultError::Crypto)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(key: &MasterKey, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if sealed.len() <= NONCE_LEN {
        return Err(VaultError::Crypto);
    }
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|_| VaultError::Crypto)?;
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| VaultError::Crypto)?;
    Ok(Zeroizing::new(plain))
}

/// 系统钥匙串里的条目坐标。`legacy_file` 是迁移前的主密钥文件；
/// 只有钥匙串读写都验证通过后，才允许删掉它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyringSpec {
    pub service: String,
    pub account: String,
    pub legacy_file: PathBuf,
}

impl KeyringSpec {
    pub fn for_app(legacy_file: PathBuf) -> Self {
        // 服务名/账户可以用环境变量改：跑独立验收或多份配置时，不能碰到真实用户的主密钥。
        // 默认值与真实应用一致。
        let service =
            std::env::var("DBHX_KEYRING_SERVICE").unwrap_or_else(|_| "com.dbhx.shell".to_string());
        let account = std::env::var("DBHX_KEYRING_ACCOUNT")
            .unwrap_or_else(|_| "vault-master-key".to_string());
        Self {
            service,
            account,
            legacy_file,
        }
    }
}

/// 主密钥来源。
#[derive(Debug, Clone)]
pub enum KeySource {
    /// 仅测试用：密钥直接躺在进程里
    Memory(MasterKey),
    /// `{config_dir}/dbhx-shell/master.key`，0600。
    /// 钥匙串不可用时的兜底（无桌面会话的 Linux、CI）。
    File(PathBuf),
    /// 系统钥匙串；首次打开时把旧 `master.key` 迁进去并删掉明文文件。
    Keyring(KeyringSpec),
}

impl KeySource {
    /// 读主密钥；`File` 源在文件不存在时创建一把新的并落盘，
    /// `Keyring` 源在两端都没有时生成一把并写进系统钥匙串。
    pub fn load_or_create(&self) -> Result<MasterKey, VaultError> {
        match self {
            KeySource::Memory(k) => Ok(k.clone()),
            KeySource::File(path) => load_or_create_file(path),
            KeySource::Keyring(spec) => load_or_create_keyring(spec),
        }
    }
}

fn load_or_create_file(path: &Path) -> Result<MasterKey, VaultError> {
    if let Some(key) = read_key_file(path)? {
        return Ok(key);
    }
    let key = MasterKey::generate();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| VaultError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
    }
    write_key_file(path, key.as_bytes())?;
    Ok(key)
}

fn read_key_file(path: &Path) -> Result<Option<MasterKey>, VaultError> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read(path).map_err(|e| VaultError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    MasterKey::from_bytes(raw).map(Some)
}

/// 钥匙串操作的窄口子。生产实现是 `keyring::Entry`，测试用内存假货——
/// 迁移逻辑里「读回校验、删明文、冲突保留」这几条最容易写错，必须在 CI 里能跑。
trait SecretEntry {
    fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyringFault>;
    fn set(&self, secret: &[u8]) -> Result<(), KeyringFault>;
}

enum KeyringFault {
    Unavailable,
    Other(String),
}

struct OsEntry(keyring::v1::Entry);

impl SecretEntry for OsEntry {
    fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyringFault> {
        match self.0.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(e) => Err(classify(e)),
        }
    }

    fn set(&self, secret: &[u8]) -> Result<(), KeyringFault> {
        self.0.set_secret(secret).map_err(classify)
    }
}

fn classify(e: keyring::v1::Error) -> KeyringFault {
    use keyring::v1::Error::*;
    match e {
        NoStorageAccess(_) | PlatformFailure(_) | NoDefaultStore => KeyringFault::Unavailable,
        other => KeyringFault::Other(other.to_string()),
    }
}

fn load_or_create_keyring(spec: &KeyringSpec) -> Result<MasterKey, VaultError> {
    let entry = keyring::v1::Entry::new(&spec.service, &spec.account)
        .map_err(|e| fault_to_error(classify(e)))?;
    let entry = OsEntry(entry);
    load_or_create_keyring_with(&entry, spec)
}

/// 规则：
/// - 钥匙串里有 → 用它；旧文件若与它相同就删（迁移收尾），不同则保留并告警。
/// - 钥匙串里没有 → 搬旧文件的，或生成新的；**写入后必须读回比对**，一致才删明文文件。
/// - 钥匙串不可用 → `KeyringUnavailable`，由调用方回退到文件源。
fn load_or_create_keyring_with(
    entry: &dyn SecretEntry,
    spec: &KeyringSpec,
) -> Result<MasterKey, VaultError> {
    match entry.get().map_err(fault_to_error)? {
        Some(bytes) => {
            let key = MasterKey::from_bytes(bytes.to_vec())?;
            reconcile_legacy_file(spec, &key);
            Ok(key)
        }
        None => {
            let key = match read_key_file(&spec.legacy_file)? {
                Some(key) => key,
                None => MasterKey::generate(),
            };
            entry.set(key.as_bytes()).map_err(fault_to_error)?;
            let stored = entry.get().map_err(fault_to_error)?;
            let Some(stored) = stored else {
                return Err(VaultError::Keyring(
                    "key was written but read back as missing".into(),
                ));
            };
            let stored = MasterKey::from_bytes(stored.to_vec())?;
            if stored.as_bytes() != key.as_bytes() {
                return Err(VaultError::Keyring(
                    "key read back from the keyring does not match".into(),
                ));
            }
            remove_legacy_file(spec)?;
            Ok(key)
        }
    }
}

/// 钥匙串命中时处理遗留文件：只有内容相同时才删。不同意味着这把钥匙可能
/// 解不开当前 vault，删掉旧文件就再没有退路了。
fn reconcile_legacy_file(spec: &KeyringSpec, key: &MasterKey) {
    match read_key_file(&spec.legacy_file) {
        Ok(Some(old)) if old.as_bytes() == key.as_bytes() => {
            if let Err(e) = std::fs::remove_file(&spec.legacy_file) {
                tracing::warn!(path = %spec.legacy_file.display(), %e, "旧主密钥文件删除失败");
            }
        }
        Ok(Some(_)) => tracing::warn!(
            path = %spec.legacy_file.display(),
            "钥匙串与遗留 master.key 内容不同，保留文件作为退路"
        ),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(path = %spec.legacy_file.display(), %e, "读遗留主密钥文件失败")
        }
    }
}

fn remove_legacy_file(spec: &KeyringSpec) -> Result<(), VaultError> {
    match std::fs::remove_file(&spec.legacy_file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(VaultError::Io {
            path: spec.legacy_file.display().to_string(),
            source: e,
        }),
    }
}

fn fault_to_error(fault: KeyringFault) -> VaultError {
    match fault {
        KeyringFault::Unavailable => VaultError::KeyringUnavailable,
        KeyringFault::Other(msg) => VaultError::Keyring(msg),
    }
}

#[cfg(unix)]
fn write_key_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).map_err(|e| VaultError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
        VaultError::Io {
            path: path.display().to_string(),
            source: e,
        }
    })
}

/// Windows 上没有 POSIX 权限位，靠的是目录 ACL（用户 profile 默认只有本用户可读）。
/// 这里刻意不做「icacls 加固」：那需要额外权限且失败模式很吵，留给安装器处理。
#[cfg(not(unix))]
fn write_key_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), VaultError> {
    std::fs::write(path, bytes).map_err(|e| VaultError::Io {
        path: path.display().to_string(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        MasterKey::generate()
    }

    #[test]
    fn seal_open_round_trips_and_uses_a_fresh_nonce_each_time() {
        let k = key();
        let a = seal(&k, b"hunter2").unwrap();
        let b = seal(&k, b"hunter2").unwrap();
        assert_ne!(a, b, "同一明文两次密封必须不同（nonce 随机）");
        assert_eq!(&*open(&k, &a).unwrap(), b"hunter2");
        assert_eq!(&*open(&k, &b).unwrap(), b"hunter2");
    }

    #[test]
    fn tampered_ciphertext_fails_instead_of_returning_garbage() {
        let k = key();
        let mut sealed = seal(&k, b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(matches!(open(&k, &sealed), Err(VaultError::Crypto)));
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let sealed = seal(&key(), b"secret").unwrap();
        // 每次 key() 都是新的一把
        assert!(matches!(open(&key(), &sealed), Err(VaultError::Crypto)));
    }

    #[test]
    fn short_input_is_not_mistaken_for_valid_ciphertext() {
        assert!(matches!(open(&key(), b"tiny"), Err(VaultError::Crypto)));
    }

    #[test]
    fn master_key_debug_never_prints_bytes() {
        let k = key();
        let rendered = format!("{k:?} {k:?}");
        assert!(
            !rendered
                .bytes()
                .any(|b| b == b'\\' && rendered.contains("x")),
            "Debug 输出像是含密钥的转义串: {rendered}"
        );
        assert!(rendered.contains("MasterKey"));
        assert_eq!(rendered.matches("0x").count(), 0);
    }

    #[test]
    fn rejects_key_of_wrong_length() {
        assert!(MasterKey::from_bytes(vec![0u8; 16]).is_err());
    }

    #[test]
    fn file_key_source_creates_once_then_reuses() {
        let dir = std::env::temp_dir().join(format!("dbhx-vault-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.key");
        let _ = std::fs::remove_file(&path);

        let src = KeySource::File(path.clone());
        let first = src.load_or_create().unwrap();
        let again = src.load_or_create().unwrap();
        assert_eq!(
            first.as_bytes(),
            again.as_bytes(),
            "第二次必须复用已落盘的密钥"
        );

        // 换一把源指向同一文件也应得到同一把
        let other = KeySource::File(path.clone()).load_or_create().unwrap();
        assert_eq!(first.as_bytes(), other.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbhx-vault-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keyring_spec(dir: &Path) -> KeyringSpec {
        KeyringSpec::for_app(dir.join("master.key"))
    }

    struct FakeEntry {
        value: std::sync::Mutex<Option<Vec<u8>>>,
        unavailable: bool,
        corrupt_on_set: bool,
    }

    impl FakeEntry {
        fn empty() -> Self {
            Self {
                value: Default::default(),
                unavailable: false,
                corrupt_on_set: false,
            }
        }

        fn holding(key: &MasterKey) -> Self {
            Self {
                value: std::sync::Mutex::new(Some(key.as_bytes().to_vec())),
                unavailable: false,
                corrupt_on_set: false,
            }
        }

        fn unavailable() -> Self {
            Self {
                value: Default::default(),
                unavailable: true,
                corrupt_on_set: false,
            }
        }

        fn corrupting() -> Self {
            Self {
                value: Default::default(),
                unavailable: false,
                corrupt_on_set: true,
            }
        }

        fn stored(&self) -> Option<Vec<u8>> {
            self.value.lock().unwrap().clone()
        }
    }

    impl SecretEntry for FakeEntry {
        fn get(&self) -> Result<Option<Zeroizing<Vec<u8>>>, KeyringFault> {
            if self.unavailable {
                return Err(KeyringFault::Unavailable);
            }
            Ok(self.stored().map(Zeroizing::new))
        }

        fn set(&self, secret: &[u8]) -> Result<(), KeyringFault> {
            if self.unavailable {
                return Err(KeyringFault::Unavailable);
            }
            let mut stored = secret.to_vec();
            if self.corrupt_on_set {
                stored[0] ^= 0xFF;
            }
            *self.value.lock().unwrap() = Some(stored);
            Ok(())
        }
    }

    #[test]
    fn keyring_migrates_legacy_file_then_deletes_the_plaintext() {
        let dir = temp_dir("kr-migrate");
        let spec = keyring_spec(&dir);
        let old = load_or_create_file(&spec.legacy_file).unwrap();
        assert!(spec.legacy_file.exists());

        let entry = FakeEntry::empty();
        let key = load_or_create_keyring_with(&entry, &spec).unwrap();

        assert_eq!(key.as_bytes(), old.as_bytes());
        assert_eq!(entry.stored().as_deref(), Some(old.as_bytes()));
        assert!(!spec.legacy_file.exists(), "迁移成功必须删掉明文文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_generates_a_key_when_neither_store_has_one() {
        let dir = temp_dir("kr-generate");
        let spec = keyring_spec(&dir);
        let entry = FakeEntry::empty();

        let key = load_or_create_keyring_with(&entry, &spec).unwrap();

        assert_eq!(entry.stored().as_deref(), Some(key.as_bytes()));
        assert!(
            !spec.legacy_file.exists(),
            "新装用户不该凭空多出一个明文密钥文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_hit_reuses_the_stored_key_and_sweeps_matching_file() {
        let dir = temp_dir("kr-hit");
        let spec = keyring_spec(&dir);
        let key = MasterKey::generate();
        write_key_file(&spec.legacy_file, key.as_bytes()).unwrap();
        let entry = FakeEntry::holding(&key);

        let opened = load_or_create_keyring_with(&entry, &spec).unwrap();

        assert_eq!(opened.as_bytes(), key.as_bytes());
        assert!(!spec.legacy_file.exists(), "内容相同的遗留文件应当被清掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_keeps_a_plaintext_file_whose_key_disagrees() {
        let dir = temp_dir("kr-conflict");
        let spec = keyring_spec(&dir);
        let keyring_key = MasterKey::generate();
        let file_key = MasterKey::generate();
        write_key_file(&spec.legacy_file, file_key.as_bytes()).unwrap();
        let entry = FakeEntry::holding(&keyring_key);

        let opened = load_or_create_keyring_with(&entry, &spec).unwrap();

        assert_eq!(opened.as_bytes(), keyring_key.as_bytes());
        assert!(
            spec.legacy_file.exists(),
            "钥匙不同的时候，删掉文件可能是唯一的退路"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unavailable_keyring_surfaces_for_the_file_fallback() {
        let dir = temp_dir("kr-unavailable");
        let spec = keyring_spec(&dir);

        assert!(matches!(
            load_or_create_keyring_with(&FakeEntry::unavailable(), &spec),
            Err(VaultError::KeyringUnavailable)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_readback_is_an_error_and_leaves_the_file_alone() {
        let dir = temp_dir("kr-readback");
        let spec = keyring_spec(&dir);
        load_or_create_file(&spec.legacy_file).unwrap();

        assert!(matches!(
            load_or_create_keyring_with(&FakeEntry::corrupting(), &spec),
            Err(VaultError::Keyring(_))
        ));
        assert!(spec.legacy_file.exists(), "没验证通过就不能删明文");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "需要桌面会话与真实系统钥匙串；手动跑 cargo test -p dbhx-vault -- --ignored"]
    fn real_keyring_round_trips_and_cleans_up() {
        let account = format!("dbhx-test-{}", std::process::id());
        let entry = keyring::v1::Entry::new("com.dbhx.shell-test", &account).unwrap();
        let key = MasterKey::generate();

        entry.set_secret(key.as_bytes()).unwrap();
        assert_eq!(entry.get_secret().unwrap(), key.as_bytes());
        entry.delete_credential().unwrap();
    }
}
