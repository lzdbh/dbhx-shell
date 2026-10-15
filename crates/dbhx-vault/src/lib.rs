//! 加密 vault + 主机密钥存储。
//!
//! 这个 crate 是明文凭据在整条链路上**唯一**被解出来的地方，因此它的边界规则是硬的：
//! - 秘密只能通过 `Vault::with_secret` 的回调借出，回调外一律是 `SecretId`；
//! - 系统的 `~/.ssh/known_hosts` 只读导入，永不写回；
//! - 云同步只在 `snapshot`/`envelope`/`sync` 这三层里换锁，出库即密文，
//!   派生出的同步密钥只以 `DerivedKey` 的形式存在，调用层拿不到裸字节；
//! - 不依赖 Tauri，所以 `cargo test` 不需要 webview，CI 能直接跑。

pub mod crypto;
pub mod envelope;
pub mod error;
pub mod hostkey;
pub mod snapshot;
pub mod store;
pub mod sync;

pub use crypto::{KeySource, MasterKey};
pub use error::VaultError;
pub use hostkey::KnownHosts;
pub use store::{DeletionKind, DeletionMark, SecretKind, Vault};
pub use sync::DerivedKey;
