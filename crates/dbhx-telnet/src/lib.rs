//! 干净的 telnet 客户端。
//!
//! 只覆盖「连上一条 TCP、把 IAC 对话处理干净、双向透传原始字节」这一件事：
//! 没有池、没有主机密钥、没有凭据概念（登录发生在流里），因此它天然满足
//! 「明文永不跨 IPC」。协议细节集中在 [`iac`]，会话装配在 [`session`]，
//! 两层都能在无网络条件下单测——`iac` 是纯函数，`session` 配一个进程内 TCP 服务器。

#![forbid(unsafe_code)]

pub mod error;
pub mod iac;
pub mod session;

pub use error::TelnetError;
pub use session::{TelnetEvent, TelnetHandle, connect};
