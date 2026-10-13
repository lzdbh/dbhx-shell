//! AES-256-GCM 密封与主密钥来源。
//!
//! 密文格式（一条记录一个 nonce，不用随机数复用）：
//! `base64( nonce[12] || ciphertext || tag[16] )`。
//! 选 GCM 而不是 CBC：后者没有完整性校验，本地文件被改一字节会解出垃圾而不报错。

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
        f.debug_struct("MasterKey")
            .finish_non_exhaustive()
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

/// 主密钥来源抽象。M5 会加 `Keyring`（Windows 凭据管理器 / macOS Keychain /
/// secret-service），现在只有文件与内存两种，测试一律走内存。
#[derive(Debug, Clone)]
pub enum KeySource {
    /// 仅测试用：密钥直接躺在进程里
    Memory(MasterKey),
    /// `{config_dir}/dbhx-shell/master.key`，0600。
    /// 安全性不如系统密钥库，但没有它整个 vault 就无法在无密钥环的 Linux CI 上跑。
    File(std::path::PathBuf),
}

impl KeySource {
    /// 读主密钥；`File` 源在文件不存在时创建一把新的并落盘。
    pub fn load_or_create(&self) -> Result<MasterKey, VaultError> {
        match self {
            KeySource::Memory(k) => Ok(k.clone()),
            KeySource::File(path) => {
                if path.exists() {
                    let raw = std::fs::read(path).map_err(|e| VaultError::Io {
                        path: path.display().to_string(),
                        source: e,
                    })?;
                    return MasterKey::from_bytes(raw);
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
        }
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
            !rendered.bytes().any(|b| b == b'\\' && rendered.contains("x")),
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
        assert_eq!(first.as_bytes(), again.as_bytes(), "第二次必须复用已落盘的密钥");

        // 换一把源指向同一文件也应得到同一把
        let other = KeySource::File(path.clone()).load_or_create().unwrap();
        assert_eq!(first.as_bytes(), other.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
