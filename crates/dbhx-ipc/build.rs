use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.ends_with("-windows-msvc") {
        return;
    }

    // tao/wry 直接导入 comctl32 的 `TaskDialogIndirect`，那是 Common Controls **6.0**
    // 才有的导出。没有 v6 manifest 时 Windows 装载 v5.82，进程在加载阶段就失败：
    // 0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND —— 测试二进制连 main 都进不去，
    // 所以看不到任何输出，只能靠 dumpbin /imports 定位。
    // 主程序由 tauri_build 负责嵌 manifest，这里只补 `cargo test` 出来的测试二进制。
    let dependency = "type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'";

    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTDEPENDENCY:{dependency}");
}
