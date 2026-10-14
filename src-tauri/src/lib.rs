//! 装配层：注册 plugin、建窗口钩子。业务一律在 crates/ 里。

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 装一个进程级 tracing sink：这是 russh/会话层英文错误**唯一**允许落地的地方
    // （IPC 回给渲染层的永远是脱敏中文）。默认 info，`russh=warn` 压掉库的握手噪声；
    // RUST_LOG 可覆盖。之前没有任何 subscriber，所有 tracing 调用都被丢进空散播器，
    // 所以后端错误在日志里完全看不见——连「连不上」的真实原因都无从查起。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,russh=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    tauri::Builder::default()
        .plugin(dbhx_ipc::plugin())
        .run(tauri::generate_context!())
        .expect("error while running dbhx shell");
}
