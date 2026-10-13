//! 会话层：transport 池、lease 引用计数、idle park、并发拨号去重。
//!
//! 边界：这一层**不认识 vault、不认识 Tauri**。凭据来自 [`plan::PlanSource`]
//! （由上层实现，它才知道密文在哪一行），连接来自 [`conn::Dialer`]（生产实现是
//! `SshConnection::connect`，测试实现是假的）。因此本 crate 的测试不需要网络，
//! 也不需要 `unsafe_code` 之外的任何特权。
//!
//! 四条从参考实现抄来的规则，写在这里是因为它们都是被真实 bug 磨出来的：
//!
//! 1. **判等要用「会话的实时 endpoint」**（[`key::TransportKey`]）。用户连上之后编辑了
//!    主机记录，若拿记录里的静态字段判等，就会误复用或误另开一条连接。
//! 2. **同一身份键的并发拨号必须只有一次**。开 OTP 的服务器被同时认证两次，就会弹两次
//!    动态码——这不是性能问题，是安全问题。做法见 `pool::Inner::acquire`。
//! 3. **最后一个 lease 释放不等于断开**。进入 idle park，TTL 到点才真断（默认 5 分钟，
//!    `Duration::ZERO` 表示保留到退出）。关掉 tab 又马上回来的人不该重输口令。
//! 4. **复用路径零凭据**。命中池就直接返回 lease，绝不问 `PlanSource`，
//!    明文因此连会话层都不进。
//!
//! 时钟用 `tokio::time::Instant` 而不是 `std::time::Instant`：TTL 与清扫任务因此可以在
//! 暂停时钟的单测里几毫秒跑完。

pub mod conn;
pub mod cwd;
pub mod error;
pub mod key;
pub mod manager;
pub mod plan;
pub mod pool;
pub mod shell;

pub use conn::{BoxFuture, Dialer, PooledConnection, SharedConnection, SshConnection, ssh_dialer};
pub use cwd::CwdTracker;
pub use error::SessionError;
pub use key::TransportKey;
pub use manager::{SessionEvent, SessionManager, SessionRecord};
pub use plan::{DialPlan, PlanSource};
pub use pool::{Lease, LeaseKind, PoolOptions, TransportPool};
pub use shell::{ExitInfo, ShellEvent, ShellSession};
