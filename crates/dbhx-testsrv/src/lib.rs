//! 进程内 SSH/SFTP 测试服务器。
//!
//! **为什么需要它**：`SubsystemStream` 是 russh 通道的半成品类型，离线伪造不出来，
//! 所以「假拨号器」只能覆盖到注册表、门控、错误翻译那一段；真正打通道的代码
//! （transport 池握手、认证、shell 通道、SFTP 报文、传输引擎）在没有服务器的情况下
//! 从未被执行过。这个 crate 用 `russh::server` + `russh_sftp::server` 把那条通道补上，
//! 让上面这些路径在 CI 里、无网络、无外部 sshd 的条件下跑真实协议。
//!
//! **它不该出现在产品里**：只作为其它 crate 的 `dev-dependencies` 存在。
//! 因此这里没有任何「生产可用」的语义——认证一律放行、VFS 在内存里、
//! 主机密钥每次启动随机生成。别把它当成一个真的服务器。
//!
//! 用法：
//! ```ignore
//! let server = TestServer::start(TestSpec::new()).await?;
//! let endpoint = LiveEndpoint { host: server.host(), port: server.port(), username: "app".into() };
//! // ……用 dbhx-ssh 拨过去……
//! server.shutdown().await;
//! ```

mod server;
mod sftp;
pub mod vfs;

pub use server::{Observed, TestServer, TestSpec};
pub use sftp::SftpHandler;
pub use vfs::{FsError, Kind, Stat, Vfs};

/// 测试服务器的主机密钥（ed25519，无口令）。
///
/// 固定而不是随机生成，是为了让 vault 侧的测试能把这条主机**预先写成已信任**：
/// 主机密钥比对的是字节级 base64，随机密钥就得先把服务端的公钥捞出来，
/// 而那个动作本身正是被测路径的一部分，不该由它自己给自己作证。
///
/// 与 `dbhx-ssh` 测试夹具里的 `PLAIN_ED25519` 是同一把密钥（同一对公私密）。
/// 这是刻意的一致性：两边的 fixture 拼在一起才凑得出「known_hosts 里的一行」。
pub const HOST_KEY_PEM: &str = concat!(
    "-----BEGIN OPENSSH PRIVATE KEY-----\n",
    "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
    "QyNTUxOQAAACCqjLiVugxar0txxix3jN0xmTy6HDFxrbnz8hDI48Ij7gAAAJBTtu8yU7bv\n",
    "MgAAAAtzc2gtZWQyNTUxOQAAACCqjLiVugxar0txxix3jN0xmTy6HDFxrbnz8hDI48Ij7g\n",
    "AAAEC6kRqKvR66+jJL8t2p/TwyEpTfMuLnJ29n7et68RRskKqMuJW6DFqvS3HGLHeM3TGZ\n",
    "PLocMXGtufPyEMjjwiPuAAAACWRiaHgtdGVzdAECAwQ=\n",
    "-----END OPENSSH PRIVATE KEY-----\n",
);

/// [`HOST_KEY_PEM`] 对应的公钥 base64（`ssh-ed25519` 的线格式 blob）。
pub const HOST_KEY_B64: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIKqMuJW6DFqvS3HGLHeM3TGZPLocMXGtufPyEMjjwiPu";

/// 主机密钥算法名，`KnownHost.algorithm` 用。
pub const HOST_KEY_ALGO: &str = "ssh-ed25519";

/// 搭一个够用的场景：家目录、一个可列举的目录、一个符号链接。
///
/// 符号链接是必须有的——「绝不跟随链接」这条规矩只有在目录项里真的躺着链接时才测得出来。
pub fn sample_fs() -> Vfs {
    let mut fs = Vfs::new();
    fs.add_dir("/home");
    fs.add_dir("/home/app");
    fs.add_dir("/home/app/logs");
    fs.add_file("/home/app/readme.md", b"hello sftp\n".to_vec());
    fs.add_file("/home/app/logs/app.log", b"line-1\nline-2\n".to_vec());
    fs.add_file("/home/app/big.bin", vec![7u8; 4096]);
    fs.add_symlink("/home/app/latest", "/home/app/logs");
    fs.add_symlink("/home/app/readme.link", "readme.md");
    fs
}
