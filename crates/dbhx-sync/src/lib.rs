//! 主机列表的云同步传输层。
//!
//! 只走 REST，一个片段文件存一份密文，不引 git 协议。理由都在根 Cargo.toml 那两段注释里：
//! libgit2 要在 build.rs 编 C（工作区为交叉编译矩阵躲过两次了），
//! 而 shell 出去调系统 git 会让 `credential store` 把令牌明文写进磁盘。
//!
//! 分工刻意切成三块，越往下越靠近真网络：
//! - `wire` 唯一的网络出口，把令牌挡在 URL 之外，超时与非 2xx 都在这一层定性；
//! - `provider` 的 URL 与请求体全是纯函数，**不碰网络**，所以两家接口的差异
//!   （Gitee 创建要 `files` 哈希、更新却是平铺字段）能在毫秒级测试里钉住；
//! - `sync` 把 vault 与 provider 接起来，只决定顺序与「这次该不该联网」。
//!
//! 离线测试覆盖不到的只有「接口真的按我们以为的形状回应」这一件事，
//! 那里留了两条 `#[ignore]` 的直连用例：`provider::tests` 验单程读写，
//! `sync::tests` 走完整两台设备。两条都自建自删，不在云上留东西。
//!
//! 令牌、同步串这类凭据一律从参数进、不长期持有，也不写进任何类型的 `Debug`。

pub mod provider;
pub mod sync;
pub mod wire;

pub use provider::{CloudBlob, ProviderError, SyncClient, SYNC_FILE_NAME};
pub use sync::{sync_once, SyncError, SyncOutcome, SyncReport};
pub use wire::{HttpTransport, Transport, WireError};
