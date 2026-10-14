fn main() {
    // dbhx plugin 的命令走 ACL：runtime 对任何 `plugin:<name>|<cmd>` 调用都要查权限，
    // 与 app 有没有自己的 ACL manifest 无关（tauri::webview 里的
    // `plugin_command.is_some() || has_app_acl_manifest || !is_local` 分支）。
    // 这里不声明权限的话，前端 invoke 会直接收到 "dbhx.ping not allowed. Plugin not found"。
    // 命令清单要与 `dbhx_ipc::COMMAND_NAMES` 保持一致，漂移由 dbhx-ipc 的
    // acl_manifest_lists_every_command 测试兜住。
    tauri_build::try_build(
        tauri_build::Attributes::new().plugin(
            "dbhx",
            tauri_build::InlinedPlugin::new()
                .commands(&[
                    "ping",
                    "ping_failure",
                    "session_open_local",
                    "session_open_remote",
                    "session_write",
                    "session_resize",
                    "session_close",
                    "session_list",
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
                    "sftp_browse",
                    "sftp_mkdir",
                    "sftp_remove",
                    "sftp_rename",
                    "sftp_close",
                    "sftp_set_following",
                    "sftp_set_editing",
                    "sftp_download",
                    "sftp_upload",
                    "transfer_cancel",
                    "transfer_running",
                ])
                .default_permission(tauri_build::DefaultPermissionRule::AllowAllCommands),
        ),
    )
    .expect("failed to run tauri-build");
}
