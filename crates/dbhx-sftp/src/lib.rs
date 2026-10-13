//! SFTP 文件面板后端：目录列举、终端目录跟随、批量传输。
//!
//! 边界：这一层**只接一条已经开好的 subsystem 裸流**（`dbhx_ssh::SubsystemStream`）。
//! 它不认识 lease、不认识 vault、不认识 Tauri——谁持有 SSH 连接、什么时候回收，
//! 都是上层（`dbhx-session` / `dbhx-ipc`）的事。因此本 crate 的测试不需要网络，
//! 也不需要真的有一台机器可以连。
//!
//! 五条从参考实现抄来的规则，它们全是被真实用户反馈磨出来的：
//!
//! 1. **远端路径永远是 POSIX**（[`path`]）。这里不用 `std::path`：客户端跑在 Windows 上，
//!    而 `/C:/Users` 这种路径里的 `:` 会被 `std::path` 当成盘符前缀算错。
//! 2. **服务端的英文原文不出面板**（[`error::SftpError::classify`]）。
//!    `STATUS` 报文里那句 `error_message` 只取 `status_code`，路径由调用方补。
//! 3. **目录跟随有优先级和门控**（[`follow`]）。弱来源不覆盖强来源；
//!    隐藏 tab 的合成「没有目录」不作废记账；传输中/编辑中不跳；
//!    进不去的目录按 `(连接, 目录)` 拉黑。
//! 4. **一条通道内并行搬运**（[`client::SftpOptions::max_inflight_ops`]）。
//!    russh-sftp 的高层 `File` 自己就排队 WRITE 应答，把并发度设够即可，
//!    不要再手搓一层流水线。
//! 5. **进度事件按时间节流**（[`transfer::PROGRESS_INTERVAL`]）。每个分块都发一条的话，
//!    webview 会被消息淹没；但终态事件一定发出去，否则条目永远挂在「传输中」。
//!
//! 本期没有做的：断点续传（需要先解决「远端文件在期间被改过」的指纹问题）。
//! 所有需要真服务的通路（russh channel ↔ `SftpSession` 的握手与报文往返）
//! 留给进程内测试服务器（任务 #17）覆盖；本 crate 的单测只碰纯逻辑。

pub mod client;
pub mod error;
pub mod follow;
pub mod path;
pub mod plan;
pub mod transfer;

pub use client::{SftpClient, SftpOptions, sort_entries};
pub use error::SftpError;
pub use follow::{Blacklist, CwdSource, Decision, FollowGate, Hold};
pub use plan::{Lister, Plan, Task, expand_local, expand_remote};
pub use transfer::{
    Batch, BatchReport, Cancel, Copier, Engine, ProgressMeter, SftpCopier, TransferOptions,
};
