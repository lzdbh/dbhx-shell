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
                .commands(&["ping", "ping_failure"])
                .default_permission(tauri_build::DefaultPermissionRule::AllowAllCommands),
        ),
    )
    .expect("failed to run tauri-build");
}
