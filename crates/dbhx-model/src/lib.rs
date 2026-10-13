//! 纯类型与纯函数层。
//!
//! 边界：不依赖 Tauri、不做 I/O、不碰网络与文件系统。
//! 这样连接池判等、lease 计数、cwd 跟随门控这类逻辑都能 `cargo test` 直接验证，
//! 不需要起 webview。

pub mod error;
pub mod session;
pub mod vault;

pub use error::{redacted, ErrorCode, IpcError};
pub use session::{
    AuthRef, ConnectSpec, Endpoint, LiveEndpoint, Protocol, Pong, PtySize, SessionId, SessionInfo,
    SessionStatus, TerminalEncoding,
};
pub use vault::{
    AuthMode, GroupMode, HostId, HostInfo, HostKeyDecision, HostRecord, IdentityId, IdentityRecord,
    KnownHost, KnownHostSource, SecretId,
};
