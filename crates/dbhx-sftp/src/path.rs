//! 远端路径的纯词法操作。
//!
//! 为什么不用 `std::path`：SFTP 线上永远是 POSIX 风格路径（Windows 版 OpenSSH 也报
//! `/C:/Users/...`），而 `std::path` 在 Windows 上会把 `\` 当分隔符、把 `C:` 当盘符前缀，
//! 于是「在 Windows 本机跑的客户端」会算出错误的远端路径。这里只做字符串处理，
//! 语义与 POSIX 一致，且完全离线可测。

/// 远端根目录。
pub const ROOT: &str = "/";

/// 以 POSIX 规则判断绝对路径（`/` 开头）。
pub fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

/// 拼一个子项。`name` 以 `/` 开头时视为绝对路径直接返回——调用方给的「名字」
/// 有时其实是一个子路径（面板的地址栏允许用户粘贴 `/a/b`）。
pub fn join(base: &str, name: &str) -> String {
    if name.is_empty() {
        return normalize(base);
    }
    if name.starts_with('/') {
        return normalize(name);
    }
    if base.is_empty() {
        return normalize(name);
    }
    if base.ends_with('/') {
        normalize(&format!("{base}{name}"))
    } else {
        normalize(&format!("{base}/{name}"))
    }
}

/// 取最后一个路径段。根目录、空串、`.` 都没有名字，返回 `None`。
pub fn basename(path: &str) -> Option<&str> {
    let trimmed = trim_trailing(path);
    if trimmed.is_empty() || trimmed == "." {
        return None;
    }
    match trimmed.rfind('/') {
        // 相对路径里也可能有分隔符：`a/b` 的名字是 `b`
        Some(idx) => trimmed.get(idx + 1..).filter(|s| !s.is_empty()),
        None => Some(trimmed),
    }
}

/// 上一级目录。已经在根上就返回 `None`，让调用方把「返回上级」按钮置灰——
/// 返回 `/` 会让按钮看起来还能点，点了却什么都没发生。
pub fn parent(path: &str) -> Option<String> {
    let normalized = normalize(path);
    if normalized == ROOT || normalized == "." {
        return None;
    }
    // 绝对路径的上一级一定存在（至少是根）
    if is_absolute(&normalized) {
        return match normalized.rfind('/') {
            Some(0) | None => Some(ROOT.to_string()),
            Some(idx) => Some(normalize(&normalized[..idx])),
        };
    }
    match normalized.rfind('/') {
        Some(idx) => Some(normalized[..idx].to_string()),
        None => Some(".".to_string()),
    }
}

/// 词法规范化：折叠 `//`、去掉 `.`、就地解 `..`、去尾斜杠。
///
/// 绝对路径越过的 `..` 一律吃掉（POSIX 里 `/..` 就是 `/`）；相对路径开头的 `..` 保留，
/// 因为 `../a` 有意义。不做任何服务端交互——真正的答案是 `SftpClient::canonicalize`，
/// 符号链接只有它解得开。
pub fn normalize(path: &str) -> String {
    let absolute = is_absolute(path);
    let mut stack: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match stack.last() {
                // 相对路径开头、或上一段本身就是 `..` 时，继续往外堆
                Some(&"..") | None if !absolute => stack.push(".."),
                Some(_) => {
                    stack.pop();
                }
                // 绝对路径且栈空：已经在根，`..` 仍是根
                None => {}
            },
            other => stack.push(other),
        }
    }
    if absolute {
        if stack.is_empty() {
            ROOT.to_string()
        } else {
            format!("/{}", stack.join("/"))
        }
    } else if stack.is_empty() {
        ".".to_string()
    } else {
        stack.join("/")
    }
}

/// 把 `~` 规范化成 `~/xxx`。
///
/// 客户端不知道远端的 home 在哪，展开是服务端的事（OpenSSH 的 sftp-server 认 `~`）。
/// 这里只保证写法统一：单独的 `~` 变成 `~/`，`~user` 这类带用户名的形式原样留着，
/// 交给服务端判断——本地猜路径只会猜错。
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return "~/".to_string();
    }
    path.to_string()
}

fn trim_trailing(path: &str) -> &str {
    let mut end = path.len();
    // 长度下界取 1：根目录的斜杠不能剪光，否则 basename("/") 会拿到空串
    while end > 1 && path.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

/// 把远端路径切成「逐段可点」的面包屑，从根开始累积。
///
/// `/var/log/app` ⇒ `[("var","/var"),("log","/var/log"),("app","/var/log/app")]`。
/// 根不在结果里：它没有名字，面板把「根」放在面包屑最左边的固定入口上。
pub fn breadcrumb(path: &str) -> Vec<(String, String)> {
    let normalized = normalize(path);
    if !is_absolute(&normalized) || normalized == ROOT {
        return Vec::new();
    }
    let mut acc = String::new();
    normalized
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|name| {
            acc.push('/');
            acc.push_str(name);
            (name.to_string(), acc.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_dots_and_duplicate_slashes() {
        assert_eq!(normalize("/var//log/./app"), "/var/log/app");
        assert_eq!(normalize("/var/log/"), "/var/log");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("."), ".");
        assert_eq!(normalize("a/b/../c"), "a/c");
    }

    #[test]
    fn dot_dot_on_absolute_path_clamps_at_root() {
        // POSIX 里 /.. 就是 /；写成空串会让后来的 join 把根当成相对路径
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("/../../etc"), "/etc");
        assert_eq!(normalize("/etc/../.."), "/");
    }

    #[test]
    fn leading_dot_dot_is_kept_for_relative_paths() {
        assert_eq!(normalize("a/../../b"), "../b");
        assert_eq!(normalize("../../b"), "../../b");
        assert_eq!(normalize("../.."), "../..");
    }

    #[test]
    fn join_never_produces_double_slash() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/var", "log"), "/var/log");
        assert_eq!(join("/var/", "log"), "/var/log");
        assert_eq!(join("/var", ""), "/var");
        assert_eq!(join("/var", "/etc/passwd"), "/etc/passwd", "绝对名字直接覆盖 base");
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("/", "/"), "/");
    }

    #[test]
    fn parent_stops_at_root_and_returns_none() {
        assert_eq!(parent("/").as_deref(), None);
        assert_eq!(parent("/etc").as_deref(), Some("/"));
        assert_eq!(parent("/var/log/app").as_deref(), Some("/var/log"));
        assert_eq!(parent("/var/log/").as_deref(), Some("/var"), "尾斜杠只是写法差异");
        assert_eq!(parent(".").as_deref(), None);
        assert_eq!(parent("a").as_deref(), Some("."));
        assert_eq!(parent("a/b").as_deref(), Some("a"));
    }

    #[test]
    fn basename_of_root_and_empty_is_none() {
        assert_eq!(basename("/"), None);
        assert_eq!(basename(""), None);
        assert_eq!(basename("."), None);
        assert_eq!(basename("/var/log"), Some("log"));
        assert_eq!(basename("/var/log/"), Some("log"));
        assert_eq!(basename("a/b"), Some("b"));
    }

    #[test]
    fn windows_sftp_paths_stay_posix() {
        // Win32-OpenSSH 报的路径以 / 开头，盘符只是第一段——不能当成 std::path 的 C: 处理
        assert_eq!(parent("/C:/Users/app").as_deref(), Some("/C:/Users"));
        assert_eq!(basename("/C:/Users/app"), Some("app"));
        assert_eq!(join("/C:/Users", "app"), "/C:/Users/app");
        assert_eq!(normalize("/C:/Users/app/.."), "/C:/Users");
    }

    #[test]
    fn breadcrumb_accumulates_from_root() {
        assert_eq!(
            breadcrumb("/var/log/app"),
            vec![
                ("var".to_string(), "/var".to_string()),
                ("log".to_string(), "/var/log".to_string()),
                ("app".to_string(), "/var/log/app".to_string()),
            ]
        );
        assert!(breadcrumb("/").is_empty());
        assert!(breadcrumb("relative/path").is_empty(), "相对路径没有可点的面包屑");
    }

    #[test]
    fn tilde_only_gets_slashed_never_expanded() {
        assert_eq!(expand_tilde("~"), "~/");
        assert_eq!(expand_tilde("~/logs"), "~/logs");
        assert_eq!(expand_tilde("~deploy"), "~deploy", "带用户名的形式交服务端判断");
        assert_eq!(expand_tilde("/etc"), "/etc");
    }

    #[test]
    fn multibyte_names_survive() {
        assert_eq!(join("/日志", "应用"), "/日志/应用");
        assert_eq!(basename("/日志/应用"), Some("应用"));
        assert_eq!(parent("/日志/应用").as_deref(), Some("/日志"));
    }
}
