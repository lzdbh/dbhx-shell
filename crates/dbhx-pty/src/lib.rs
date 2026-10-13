//! 本地 PTY 会话（Windows ConPTY / Unix forkpty），基于 portable-pty。
//!
//! 边界：只依赖 portable-pty / tokio，**不依赖 Tauri、不读配置**。
//! 对外契约刻意与 `dbhx_ssh::Shell` 对齐——「送原始字节、收原始字节、能 resize」，
//! 会话层因此可以用同一套代码驱动本地终端和远端 shell，差别只在「谁在解释退出」。
//!
//! 四个非显然的实现约束，都写在 `session` 模块里：
//! * portable-pty 全是阻塞 API，读写与退出探测各占一个 `spawn_blocking` 任务。
//! * `MasterPty` 不是 `Sync`，尺寸变化只能投递给独占 master 的任务。
//! * 输出通道无界（背压会冻住子进程），输入通道有界（键盘输入本该瞬时完成）。
//! * **ConPTY 会发 `CSI 6 n` 问光标位置，并且在拿到回答前扣住子进程的全部输出**，
//!   所以读端必须代答一次，否则首屏永远出不来（实测过，见 `DSR_QUERY`）。

pub mod error;
pub mod session;
pub mod spec;

pub use error::PtyError;
pub use session::{LocalSession, PtyEvent, PtyExit};
pub use spec::LocalSpec;
