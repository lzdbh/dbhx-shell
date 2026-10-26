//! `dbhx` Tauri v2 plugin：命令契约的唯一来源。
//!
//! 做成 plugin 而非裸 command 的原因：命令自动获得 `plugin:dbhx|` 命名空间，
//! 并可用 capabilities 收紧到具体窗口；同时 `tauri-specta` 能据此导出 TS，
//! 前端类型与 Rust 同源，杜绝手写契约漂移。
//!
//! 装配全部发生在 [`plugin`] 的 setup 里：命令层唯一被托管的状态是 `Arc<Core>`，
//! 除此之外应用没有第二个全局。两条常驻任务从这里出发：
//!
//! - 事件路由：内部总线 ⇒ `app.emit`。总线只有一个读者，所以事件顺序与产生顺序一致。
//! - 会话 Hub：`SessionManager` 的事件流 ⇒ 总线（`hub::spawn`），同样只有一个读者，
//!   两个读者会随机丢一半终端字节。

pub mod bytes;
pub mod commands;
pub mod events;
// `pub` 只为集成测试：真服务器的端到端用例要自己起唯一的会话事件读者。
pub mod hub;
/// 会话日志落点表。`pub` 是为了让 `hub` 与命令层共用同一份类型，不是给外部的入口。
pub mod logsink;
/// 连接后自动探测远端系统。`pub` 只为集成测试能对着 `PROBE_COMMAND` 断言。
pub mod osdetect;
mod plansrc;
pub mod state;
#[cfg(test)]
mod test_keys;

pub use commands::{ping, ping_failure};
pub use state::Core;

use std::sync::Arc;

use tauri::Manager;
use tokio::sync::mpsc;
use tauri_specta::{collect_commands, collect_events};

/// plugin 名，同时也是命令命名空间。builder 与 tauri plugin 必须用同一个值，
/// 否则生成的 `invoke("plugin:xxx|...")` 与运行时注册的名字对不上。
const PLUGIN_NAME: &str = "dbhx";

/// 命令的 snake_case 名字清单。
///
/// 这份清单本身不是运行时真值（真值是 `collect_commands!`），但它是两份必须同步的
/// 下游配置的对照基准：`src-tauri/build.rs` 用它生成 ACL 权限，`bindings.ts` 用它
/// 校验 invoke 名字的命名空间。下面的测试负责抓漂移。
pub const COMMAND_NAMES: &[&str] = &[
    "ping",
    "ping_failure",
    "session_open_local",
    "session_open_remote",
    "session_write",
    "session_resize",
    "session_close",
    "session_list",
    "session_log_start",
    "session_log_stop",
    "vault_hosts",
    "vault_upsert_host",
    "vault_remove_host",
    "vault_set_password",
    "vault_reveal_password",
    "vault_identities",
    "vault_add_identity",
    "vault_remove_identity",
    "hostkeys_list",
    "hostkeys_forget",
    "hostkeys_import",
    "request_reply",
    "vault_lock_status",
    "vault_lock",
    "vault_unlock",
    "vault_set_lock_pin",
    "vault_clear_lock_pin",
    "sftp_open_for_session",
    "sftp_open_for_host",
    "sftp_open_elevated",
    "sftp_browse",
    "sftp_mkdir",
    "sftp_create_file",
    "sftp_chmod",
    "sftp_remove",
    "sftp_rename",
    "sftp_close",
    "sftp_set_following",
    "sftp_set_editing",
    "sftp_download",
    "sftp_upload",
    "transfer_cancel",
    "transfer_running",
    "local_home",
    "local_drives",
    "local_pick_dir",
    "local_list",
    "local_mkdir",
    "local_create_file",
    "local_remove",
    "local_rename",
    "local_stat",
    "local_open",
    "local_edit_dir",
    "local_clipboard_read",
    "local_clipboard_write",
    "open_file_window",
    "sync_status",
    "sync_set_str",
    "sync_new_str",
    "sync_set_token",
    "sync_set_provider",
    "sync_set_fragment",
    "sync_set_private_keys",
    "sync_now",
    "sync_recreate",
];

/// 命令清单的唯一构造点。运行时注册与 TS 导出都走它，保证不会分叉。
///
/// 命令本身不泛型化（没有 `R: Runtime` 参数），所以 `collect_commands!` 里不要写 `::<tauri::Wry>`。
pub fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .plugin_name(PLUGIN_NAME)
        // 默认会把 u64/i64/usize 当 BigInt 拒绝导出，而 serde_json 传的是普通 number。
        // 本项目过界的整数只有 epoch 毫秒和字节计数，都远小于 2^53，所以统一按 number 导出。
        // 可能超过 2^53 的量（累计传输字节、纳秒时间戳）必须改走 string。
        .dangerously_cast_bigints_to_number()
        .commands(collect_commands![
            commands::ping,
            commands::ping_failure,
            commands::session::session_open_local,
            commands::session::session_open_remote,
            commands::session::session_write,
            commands::session::session_resize,
            commands::session::session_close,
            commands::session::session_list,
            commands::log::session_log_start,
            commands::log::session_log_stop,
            commands::vault::vault_hosts,
            commands::vault::vault_upsert_host,
            commands::vault::vault_remove_host,
            commands::vault::vault_set_password,
            commands::vault::vault_reveal_password,
            commands::vault::vault_identities,
            commands::vault::vault_add_identity,
            commands::vault::vault_remove_identity,
            commands::vault::hostkeys_list,
            commands::vault::hostkeys_forget,
            commands::vault::hostkeys_import,
            commands::vault::request_reply,
            commands::vault::vault_lock_status,
            commands::vault::vault_lock,
            commands::vault::vault_unlock,
            commands::vault::vault_set_lock_pin,
            commands::vault::vault_clear_lock_pin,
            commands::sftp::sftp_open_for_session,
            commands::sftp::sftp_open_for_host,
            commands::sftp::sftp_open_elevated,
            commands::sftp::sftp_browse,
            commands::sftp::sftp_mkdir,
            commands::sftp::sftp_create_file,
            commands::sftp::sftp_chmod,
            commands::sftp::sftp_remove,
            commands::sftp::sftp_rename,
            commands::sftp::sftp_close,
            commands::sftp::sftp_set_following,
            commands::sftp::sftp_set_editing,
            commands::transfer::sftp_download,
            commands::transfer::sftp_upload,
            commands::transfer::transfer_cancel,
            commands::transfer::transfer_running,
            commands::local::local_home,
            commands::local::local_drives,
            commands::local::local_pick_dir,
            commands::local::local_list,
            commands::local::local_mkdir,
            commands::local::local_create_file,
            commands::local::local_remove,
            commands::local::local_rename,
            commands::local::local_stat,
            commands::local::local_open,
            commands::local::local_edit_dir,
            commands::clipboard::local_clipboard_read,
            commands::clipboard::local_clipboard_write,
            commands::local::open_file_window,
            commands::sync::sync_status,
            commands::sync::sync_set_str,
            commands::sync::sync_new_str,
            commands::sync::sync_set_token,
            commands::sync::sync_set_provider,
            commands::sync::sync_set_fragment,
            commands::sync::sync_set_private_keys,
            commands::sync::sync_now,
            commands::sync::sync_recreate,
        ])
        .events(collect_events![
            events::TermData,
            events::SessionCwd,
            events::SessionEnded,
            events::SessionLogStopped,
            events::HostKeyPending,
            events::HostKeyConflict,
            events::PassphraseRequest,
            events::SftpCwdChanged,
            events::SftpFollowed,
            events::TransferProgressEvent,
            events::TransferBatchEnded,
            events::VaultChanged,
        ])
}

/// 装配 plugin。
///
/// `mount_events` 必须在任何 `emit` 之前调用，否则事件名没有注册表、前端 `listen`
/// 收不到东西却不报错；所以它和状态托管写在同一个 setup 里，不存在「命令能用但事件不能发」的中间态。
pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    let builder = specta_builder();
    let handler = builder.invoke_handler();

    tauri::plugin::Builder::new(PLUGIN_NAME)
        .setup(move |app, _api| {
            builder.mount_events(app);

            let dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("无法解析应用数据目录：{e}"))?;
            let (tx, rx) = mpsc::channel::<events::BusEvent>(state::BUS_QUEUE);
            let core = state::Core::production(dir, tx).map_err(|e| format!("命令层装配失败：{}", e.message))?;
            app.manage(Arc::clone(&core));

            // 事件路由：总线只有一个读者。用 spawn 而不是 await，setup 必须马上返回。
            let app_for_router = app.clone();
            tauri::async_runtime::spawn(async move {
                let mut rx = rx;
                while let Some(event) = rx.recv().await {
                    event.dispatch(&app_for_router);
                }
            });

            // 会话 Hub 只有一个读者，重复接管会丢字节，所以它返回 false 时只记一条日志。
            if !hub::spawn(core) {
                tracing::warn!("会话事件流已被接管，终端输出可能丢失");
            }
            Ok(())
        })
        .invoke_handler(handler)
        .build()
}

/// 前端绑定导出路径，相对 workspace 根。
///
/// 该文件是生成物但**要提交**：CI 会重新生成并 diff，用来抓 Rust 与 TS 的契约漂移。
#[cfg(test)]
const BINDINGS_REL: &str = "frontend/src/lib/ipc/bindings.ts";

#[cfg(test)]
mod codegen {
    use super::*;

    /// 只看契约区（截掉 tauri-specta 自己的 runtime 样板，那里合法地有一处 `e as any`），
    /// 丢掉注释行（Rust doc comment 会原样进 JSDoc，里面的中文可能含 "any" 字样），
    /// 再按标识符边界找 `any` —— 裸 `contains("any")` 会误伤 company/many 这类词。
    fn contract_tokens(bindings: &str) -> Vec<String> {
        let contract = bindings
            .split("/* Tauri Specta runtime */")
            .next()
            .unwrap_or(bindings);
        contract
            .lines()
            .map(str::trim)
            .filter(|l| !(l.starts_with("///") || l.starts_with("//!") || l.starts_with("/*") || l.starts_with('*') || l.starts_with("//")))
            .flat_map(|line| {
                line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    /// `cargo test -p dbhx-ipc` 即重新生成前端绑定。
    #[test]
    fn export_typescript_bindings() {
        let target = repo_root().join(BINDINGS_REL);
        std::fs::create_dir_all(target.parent().expect("bindings dir"))
            .expect("create bindings dir");

        specta_builder()
            .export(specta_typescript::Typescript::default(), &target)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", target.display()));

        let content = std::fs::read_to_string(&target)
            .unwrap_or_else(|e| panic!("cannot read back {}: {e}", target.display()));

        for needle in ["ErrorCode", "Pong", "LiveEndpoint"] {
            assert!(content.contains(needle), "bindings.ts missing `{needle}`");
        }
        // plugin 命名空间必须进 invoke 名字，否则前端调用根本路由不到 handler；
        // 同时这也证明 collect_commands! 里每条命令都真的导出了。
        for name in COMMAND_NAMES {
            let invoke = format!("\"plugin:{PLUGIN_NAME}|{name}\"");
            assert!(
                content.contains(&invoke),
                "bindings.ts 缺 {invoke} —— 命令没进 builder 或 plugin_name 不一致"
            );
        }
        // 事件的运行时名字是 `plugin:dbhx:<kebab>`，前端不算字面量但生成绑定要落盘，
        // 事件名没进 TS 就说明 collect_events! 漏了一条。
        for event in [
            "term-data",
            "session-cwd",
            "session-ended",
            "host-key-pending",
            "host-key-conflict",
            "passphrase-request",
            "sftp-cwd-changed",
            "sftp-followed",
            "transfer-progress",
            "transfer-batch-ended",
            "vault-changed",
        ] {
            assert!(content.contains(event), "bindings.ts 缺事件 {event}");
        }
        // 契约断言：宁可生成失败，也不要悄悄降级成 any
        assert!(
            !contract_tokens(&content).iter().any(|t| t == "any"),
            "bindings.ts 契约区出现 `any` —— 类型生成降级，必须排查"
        );
    }

    /// plugin 命令一律过 ACL，权限清单由 `src-tauri/build.rs` 的 InlinedPlugin 生成。
    /// 那条命令列表是手写的副本，漏一条前端就只会收到 "not allowed. Plugin not found"，
    /// 而且只在运行时炸，编译期无声——所以在这里挡住。
    #[test]
    fn acl_manifest_lists_every_command() {
        let build_rs = repo_root().join("src-tauri/build.rs");
        let content = std::fs::read_to_string(&build_rs)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", build_rs.display()));

        for name in COMMAND_NAMES {
            let quoted = format!("\"{name}\"");
            assert!(
                content.contains(&quoted),
                "src-tauri/build.rs 的 InlinedPlugin::commands 缺 {name}，\
                 该命令不会被授权，前端 invoke 会被 runtime 拒绝"
            );
        }
    }
}
