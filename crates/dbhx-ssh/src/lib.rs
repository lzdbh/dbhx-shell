//! SSH 连接层：拨号、主机密钥裁决、认证、通道。
//!
//! 边界：只依赖 russh / tokio，**不依赖 Tauri、不读配置文件**。
//! 凭据由调用方（会话层从 vault 取）以 `Credential` 递进来，明文只在本 crate 的
//! 认证函数内部短暂存在；russh 自己的 `Zeroizing` 负责用完即焚。
//!
//! 三处必须知道的实现约束，都在对应模块里写了原因：
//! * `transport`：`client::Handle` 不 `Clone` 且 drop 即断连，所以用 actor 独占。
//! * `channel`：`request_*` 返回 `Ok` 不代表服务器接受，必须自己 `wait()` 到 Success。
//! * `auth`：RSA 要协商 hash，纯密码认证要能降级到 keyboard-interactive。
//!
//! 提权看文件走 `sudo_sftp`：SFTP 子系统的人身份在认证那一刻就定了，所以那是
//! 「exec 一条跑 sudo 的前台 sftp-server，把通道字节当 SFTP 线」的独立形态。

pub mod auth;
pub mod channel;
pub mod config;
pub mod error;
pub mod handler;
pub mod hostkey;
pub mod sudo_sftp;
pub mod transport;

pub use auth::{Credential, key_is_encrypted};
pub use channel::{Chunk, Exec, ExecOutput, Shell, ShellReader, ShellWriter, SplitChannel};
pub use config::DialOptions;
pub use error::SshError;
pub use handler::{CloseReason, SessionState, SshEvent};
pub use hostkey::{pattern_of, ServerKey};
pub use transport::{
    wait_until_closed, HostKeyVerifier, RejectAll, SshTransport, TrustAll,
};

/// 给会话层与 dbhx-sftp 用的裸流类型别名，避免它们到处写 russh 的路径。
pub type SubsystemStream = russh::ChannelStream<russh::client::Msg>;

/// 测试夹具：由 `ssh-keygen -t ed25519` 真实生成，公网部分一并写死。
///
/// 用固定 key 而不是随机生成，是因为测试要断言的是**与 OpenSSH 的字节级一致性**——
/// 随机密钥只能自检，固定密钥能证明我们解析的就是 ssh-keygen 产出的那个东西。
#[cfg(test)]
pub(crate) mod fixtures {
    /// 无口令，注释 `dbhx-test`
    pub const PLAIN_ED25519: &str = concat!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
        "QyNTUxOQAAACCqjLiVugxar0txxix3jN0xmTy6HDFxrbnz8hDI48Ij7gAAAJBTtu8yU7bv\n",
        "MgAAAAtzc2gtZWQyNTUxOQAAACCqjLiVugxar0txxix3jN0xmTy6HDFxrbnz8hDI48Ij7g\n",
        "AAAEC6kRqKvR66+jJL8t2p/TwyEpTfMuLnJ29n7et68RRskKqMuJW6DFqvS3HGLHeM3TGZ\n",
        "PLocMXGtufPyEMjjwiPuAAAACWRiaHgtdGVzdAECAwQ=\n",
        "-----END OPENSSH PRIVATE KEY-----\n",
    );

    /// `ssh-keygen` 里这份的公钥列，逐字节比对用
    pub const PLAIN_ED25519_B64: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIKqMuJW6DFqvS3HGLHeM3TGZPLocMXGtufPyEMjjwiPu";

    /// 口令 `dbhx-pass`，aes256-ctr + bcrypt
    pub const ENC_ED25519: &str = concat!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBqrp7FeO\n",
        "amntYLSx8Hg/CzAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIJW61///nXWvtSuR\n",
        "FtwyrH6cAkreKErdHx+F7/j3ljtvAAAAkNBOsXyQNadqhliG5u7xkQzS+v7wqQGaoKtBuN\n",
        "zAVHImMO4D+XPbjDjxeiQMPQCIWjSw0eTEGx9KIs6aFunQNClVNTgNeUVh2oSudSlswCMn\n",
        "UhBbUe8nPE3RgZZatuTyVajhcnridkJxu52r3ykpWUqwNfsLUJmY8tmeKOn/Boy3Dhzjef\n",
        "tb3TRU+T2Zfvdu8A==\n",
        "-----END OPENSSH PRIVATE KEY-----\n",
    );

    pub const ENC_ED25519_PASSPHRASE: &str = "dbhx-pass";

    pub const ENC_ED25519_B64: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIJW61///nXWvtSuRFtwyrH6cAkreKErdHx+F7/j3ljtv";

    pub const KNOWN_HOSTS_LINE: &str = "db1.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKqMuJW6DFqvS3HGLHeM3TGZPLocMXGtufPyEMjjwiPu";
}
