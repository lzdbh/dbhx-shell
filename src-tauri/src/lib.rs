//! 装配层：注册 plugin、建窗口钩子。业务一律在 crates/ 里。

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(dbhx_ipc::plugin())
        .run(tauri::generate_context!())
        .expect("error while running dbhx shell");
}
