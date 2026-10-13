//! 把 russh 的服务器密钥投影成可入库、可比对的形式。
//!
//! 只保留 known_hosts 需要的两列（算法名 + base64 blob）和一个人类可读指纹。
//! **不保留原始 `PublicKey`**：它不可序列化，而且一旦进池就会把整条连接的生命周期拖住。
//!
//! 指纹用 `SHA256:` 前缀的 base64（无 padding），与 OpenSSH 显示的一致，
//! 这样用户拿 `ssh-keygen -lf` 核对时看到的字符串是同一个。

use dbhx_model::{KnownHost, KnownHostSource};
use russh::keys::{HashAlg, PublicKey, ssh_key::Error as SshKeyError};

/// 服务器在 `check_server_key` 里递过来的密钥，已经拆成纯数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerKey {
    /// 例如 `ssh-ed25519`、`rsa-sha2-512`——直接就是 known_hosts 第二列
    pub algorithm: String,
    /// known_hosts 第三列
    pub base64_key: String,
    /// `SHA256:...`，只给 UI 看，不参与判等
    pub fingerprint: String,
    /// 服务器用 CA 证书自证身份（`*-cert-v01@openssh.com`）
    pub is_certificate: bool,
}

impl ServerKey {
    pub fn from_public_key(key: &PublicKey, is_certificate: bool) -> Result<Self, SshKeyError> {
        // `to_openssh` 输出的正是 `<算法名> <base64> [comment]`，
        // 前两段就是 known_hosts 的一行，不需要我们自己拼 base64。
        let line = key.to_openssh()?;
        let mut parts = line.split_whitespace();
        let algorithm = parts.next().unwrap_or_default().to_string();
        let base64_key = parts.next().unwrap_or_default().to_string();
        if base64_key.is_empty() {
            return Err(SshKeyError::FormatEncoding);
        }
        Ok(Self {
            algorithm,
            base64_key,
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            is_certificate,
        })
    }

    /// 落成一条 known_hosts 记录。`pattern` 带端口时必须加方括号，这是 OpenSSH 的规矩。
    pub fn to_known_host(&self, host: &str, port: u16, source: KnownHostSource) -> KnownHost {
        KnownHost {
            pattern: pattern_of(host, port),
            algorithm: self.algorithm.clone(),
            base64_key: self.base64_key.clone(),
            added_at_ms: dbhx_model::now_ms(),
            source,
        }
    }

    /// 与已存记录判等：只比算法 + blob。指纹只是展示用的别名。
    pub fn matches(&self, stored: &KnownHost) -> bool {
        stored.algorithm == self.algorithm && stored.base64_key == self.base64_key
    }
}

pub fn pattern_of(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::PrivateKey;
    use crate::fixtures::{
        ENC_ED25519, ENC_ED25519_B64, KNOWN_HOSTS_LINE, PLAIN_ED25519, PLAIN_ED25519_B64,
    };

    fn projected(pem: &str) -> ServerKey {
        let key = PrivateKey::from_openssh(pem).unwrap();
        ServerKey::from_public_key(key.public_key(), false).unwrap()
    }

    #[test]
    fn projection_equals_what_ssh_keygen_wrote() {
        // 这条是真正的互操作断言：我们存的 base64 必须与 ssh-keygen 输出的公钥列逐字节相同，
        // 否则用户手工把我们的记录粘进 ~/.ssh/known_hosts 就对不上。
        let k = projected(PLAIN_ED25519);
        assert_eq!(k.algorithm, "ssh-ed25519");
        assert_eq!(k.base64_key, PLAIN_ED25519_B64);
        assert!(!k.is_certificate);
        assert!(k.fingerprint.starts_with("SHA256:"), "{}", k.fingerprint);
    }

    #[test]
    fn projection_survives_an_openssh_line_round_trip() {
        let k = projected(ENC_ED25519);
        assert_eq!(k.base64_key, ENC_ED25519_B64);
        let line = format!("{} {}", k.algorithm, k.base64_key);
        let back = PublicKey::from_openssh(&line).unwrap();
        assert_eq!(ServerKey::from_public_key(&back, false).unwrap(), k);
    }

    #[test]
    fn to_known_host_writes_a_parsable_line() {
        let k = projected(PLAIN_ED25519);
        let rec = k.to_known_host("db1.example.com", 22, KnownHostSource::UserConfirmed);
        let line = format!("{} {} {}", rec.pattern, rec.algorithm, rec.base64_key);
        assert_eq!(line, KNOWN_HOSTS_LINE, "生成的行必须与 ssh-keygen -H 之外的普通行同形");
    }

    #[test]
    fn pattern_brackets_the_host_only_off_default_port() {
        assert_eq!(pattern_of("db1", 22), "db1");
        assert_eq!(pattern_of("db1", 2222), "[db1]:2222");
    }

    #[test]
    fn match_is_by_blob_not_by_fingerprint_alias() {
        let k = projected(PLAIN_ED25519);
        let stored = k.to_known_host("db1", 2222, KnownHostSource::UserConfirmed);
        assert!(k.matches(&stored));
        assert_eq!(stored.pattern, "[db1]:2222");

        let other = projected(ENC_ED25519);
        assert!(!other.matches(&stored), "同一算法的不同密钥不能判成同一台");
    }

    #[test]
    fn a_certificate_projects_to_its_inner_key_and_is_flagged() {
        // CA 签发的服务器证书：known_hosts 里存的是证书内层的公钥本体，
        // 但必须带上 is_certificate，否则 UI 会把「CA 已验证」误显示成「TOFU 首次信任」。
        let line = format!("ssh-ed25519 {PLAIN_ED25519_B64}");
        let k = ServerKey::from_public_key(&PublicKey::from_openssh(&line).unwrap(), true).unwrap();
        assert_eq!(k.base64_key, PLAIN_ED25519_B64);
        assert!(k.is_certificate);
    }
}
