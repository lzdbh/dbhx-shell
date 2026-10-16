//! 主机列表的云同步传输层。
//!
//! 只走 REST，一个片段文件存一份密文，不引 git 协议。理由都在根 Cargo.toml 那两段注释里：
//! libgit2 要在 build.rs 编 C（工作区为交叉编译矩阵躲过两次了），
//! 而 shell 出去调系统 git 会让 `credential store` 把令牌明文写进磁盘。
//!
//! 分工刻意切成两半：
//! - `wire` 唯一的网络出口，把令牌挡在 URL 之外，超时与非 2xx 都在这一层定性；
//! - `provider` 的 URL 与请求体全是纯函数，**不碰网络**，所以两家接口的差异
//!   （Gitee 创建要 `files` 哈希、更新却是平铺字段）能在毫秒级测试里钉住。
//! 真联网的路径另外只留一条 `#[ignore]` 的直连测试，见 `wire::tests`。
//!
//! 令牌、同步串这类凭据一律从参数进、不长期持有，也不写进任何类型的 `Debug`。

pub mod provider;
pub mod wire;

pub use provider::{CloudBlob, ProviderError, SyncClient, SYNC_FILE_NAME};
pub use wire::{HttpTransport, Transport, WireError};
