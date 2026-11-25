//! 命令层。按「前端菜单的形状」分文件：会话、文件面板、传输、主机库。
//!
//! 每个文件里的命令都是薄壳：真正的逻辑在 `*_impl`（只吃 `&Core`，不需要 Tauri 运行时），
//! `#[tauri::command]` 那层负责解 `State`、转参数、把错误翻成 `IpcError`。
//! 拆两层的唯一目的是**能在没有 webview 的测试里跑完全部命令**。

pub mod clipboard;
pub mod local;
pub mod log;
pub mod session;
pub mod sftp;
pub mod sync;
pub mod transfer;
pub mod vault;
