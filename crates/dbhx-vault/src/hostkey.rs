//! 应用内 `known_hosts`。
//!
//! 语义按 OpenSSH 的行格式来（`pattern-list <algo> <base64>`），这样用户手工排查时
//! 看到的就是熟悉的东西；但**绝不写系统的 `~/.ssh/known_hosts`** —— 那个文件由用户的
//! `ssh` 客户端负责，导入时只读，理由见 `import_readonly`。

use std::path::{Path, PathBuf};

use dbhx_model::{HostKeyDecision, KnownHost, KnownHostSource};

use crate::error::VaultError;
use crate::store::now_ms;

#[derive(Debug, Clone)]
pub struct KnownHosts {
    path: PathBuf,
    entries: Vec<KnownHost>,
}

impl KnownHosts {
    /// 文件不存在视为空库；行解析失败**跳过该行并记录**，不让一行坏数据毁掉整份信任列表。
    pub fn load(dir: &Path) -> Result<Self, VaultError> {
        let path = dir.join("known_hosts");
        let mut skipped = 0usize;
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path).map_err(|e| VaultError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
            parse_lines(&raw, KnownHostSource::UserConfirmed, &mut skipped)
        } else {
            Vec::new()
        };
        if skipped > 0 {
            tracing::warn!(skipped, "known_hosts 有 {skipped} 行无法解析，已忽略");
        }
        Ok(Self { path, entries })
    }

    pub fn entries(&self) -> &[KnownHost] {
        &self.entries
    }

    /// 查一台主机当前存的密钥。返回第一条匹配（OpenSSH 允许同一 host 多条不同算法）。
    pub fn find(&self, host: &str, port: u16, algorithm: &str) -> Option<&KnownHost> {
        self.entries.iter().find(|e| {
            e.algorithm == algorithm && pattern_matches(&e.pattern, host, port)
        })
    }

    /// 校验 presented，产出 UI 能直接分支的结论。
    ///
    /// `Unknown` 与 `Changed` 必须是不同类型而不是同一个 bool：
    /// 前者给「确认并保存」，后者只能硬阻断（中间人风险）。
    pub fn validate(&self, presented: &KnownHost) -> HostKeyDecision {
        let (host, port) = split_pattern(&presented.pattern);
        match self.find(&host, port, &presented.algorithm) {
            None => HostKeyDecision::Unknown {
                pending: Box::new(presented.clone()),
            },
            Some(stored) if stored.base64_key == presented.base64_key => HostKeyDecision::Trusted,
            Some(stored) => HostKeyDecision::Changed {
                stored: Box::new(stored.clone()),
                presented: Box::new(presented.clone()),
            },
        }
    }

    /// 写入/替换一条信任。同 (pattern, algorithm) 的旧记录被覆盖——
    /// 用户明知密钥变了仍然确认时，旧密钥必须消失，否则下次还是 Changed。
    pub fn trust(&mut self, host: KnownHost) {
        self.entries.retain(|e| {
            !(e.pattern == host.pattern && e.algorithm == host.algorithm)
        });
        self.entries.push(host);
    }

    pub fn forget(&mut self, host: &str, port: u16, algorithm: &str) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|e| !(e.algorithm == algorithm && pattern_matches(&e.pattern, host, port)));
        before - self.entries.len()
    }

    /// 只落盘。**系统文件永不被写回**：调用方只能拿到本实例。
    pub fn save(&self) -> Result<(), VaultError> {
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(&format!("{} {} {}\n", e.pattern, e.algorithm, e.base64_key));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| VaultError::Io {
                path: dir.display().to_string(),
                source: err,
            })?;
        }
        let tmp = self.path.with_extension("hosts.tmp");
        std::fs::write(&tmp, out.as_bytes()).map_err(|err| VaultError::Io {
            path: tmp.display().to_string(),
            source: err,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|err| VaultError::Io {
            path: self.path.display().to_string(),
            source: err,
        })
    }

    /// 从系统 `~/.ssh/known_hosts` 只读导入。
    ///
    /// 两个刻意的限制：
    /// 1. 不删除、不修改源文件，也不写回去（用户可能同时用 `ssh` 命令行，
    ///    我们改他的信任列表等于跨应用越权）。
    /// 2. `|1|salt|hash` 形式的哈希行**跳过**：匹配它需要 HMAC-SHA1，
    ///    为省两个依赖而牺牲安全语义不划算，代价是这类主机第一次连会走 Unknown 确认。
    pub fn import_readonly(&mut self, source: &Path) -> Result<usize, VaultError> {
        let raw = std::fs::read_to_string(source).map_err(|e| VaultError::Io {
            path: source.display().to_string(),
            source: e,
        })?;
        let mut skipped = 0usize;
        let imported = parse_lines(&raw, KnownHostSource::Imported, &mut skipped);
        let mut added = 0usize;
        for entry in imported {
            let exists = self.entries.iter().any(|e| {
                e.pattern == entry.pattern && e.algorithm == entry.algorithm
            });
            if !exists {
                self.entries.push(entry);
                added += 1;
            }
        }
        Ok(added)
    }
}

/// 把 known_hosts 的 pattern 拆成可比较的 (host, port)。
/// OpenSSH 对非 22 端口写作 `[host]:port`，22 端口直接写裸 host。
/// 逗号分隔的多 pattern 行取第一个作为「这条记录代表谁」，匹配仍走 `pattern_matches`。
fn split_pattern(pattern_list: &str) -> (String, u16) {
    let first = pattern_list.split(',').next().unwrap_or("").trim();
    if let Some(rest) = first.strip_prefix('[') {
        let inside = rest.split(']').next().unwrap_or("").to_string();
        let port = rest
            .split(']')
            .nth(1)
            .and_then(|s| s.strip_prefix(':'))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(22);
        (inside, port)
    } else {
        (first.to_string(), 22)
    }
}

fn pattern_matches(pattern_list: &str, host: &str, port: u16) -> bool {
    let want = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    pattern_list
        .split(',')
        .any(|p| glob_match(p.trim(), &want))
}

/// 只要 `*` / `?` 两种通配（与 OpenSSH 的 pattern 语义一致），
/// 回溯版实现，模式长度受 known_hosts 行约束，性能不用考虑。
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            ti = mark + 1;
            mark = ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn parse_lines(raw: &str, source: KnownHostSource, skipped: &mut usize) -> Vec<KnownHost> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("@") {
            // @cert-authority / @revoked：本App不支持 CA 认证，明确跳过而不是当普通行解析
            *skipped += 1;
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(pattern), Some(algorithm), Some(base64_key)) =
            (parts.next(), parts.next(), parts.next())
        else {
            *skipped += 1;
            continue;
        };
        if pattern.starts_with("|1|") {
            *skipped += 1;
            continue;
        }
        out.push(KnownHost {
            pattern: pattern.to_string(),
            algorithm: algorithm.to_string(),
            base64_key: base64_key.to_string(),
            added_at_ms: now_ms(),
            source,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pattern: &str, algorithm: &str, key: &str) -> KnownHost {
        KnownHost {
            pattern: pattern.into(),
            algorithm: algorithm.into(),
            base64_key: key.into(),
            added_at_ms: 0,
            source: KnownHostSource::UserConfirmed,
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbhx-hostkeys-{tag}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_port_matches_bare_host_and_other_port_needs_brackets() {
        assert!(pattern_matches("db1.example.com", "db1.example.com", 22));
        assert!(!pattern_matches("db1.example.com", "db1.example.com", 2222));
        assert!(pattern_matches("[db1.example.com]:2222", "db1.example.com", 2222));
        assert!(pattern_matches("*.example.com,[short]:2200", "x.example.com", 22));
        assert!(pattern_matches("*.example.com,[short]:2200", "short", 2200));
    }

    #[test]
    fn unknown_and_changed_are_distinguishable() {
        let mut kh = KnownHosts {
            path: dir("validate").join("known_hosts"),
            entries: vec![entry("db1", "ssh-ed25519", "AAAOld")],
        };
        let presented = entry("db1", "ssh-ed25519", "AAAnew");
        assert!(matches!(
            kh.validate(&presented),
            HostKeyDecision::Changed { .. }
        ));
        let other = entry("db2", "ssh-ed25519", "AAA");
        assert!(matches!(kh.validate(&other), HostKeyDecision::Unknown { .. }));
        let same = entry("db1", "ssh-ed25519", "AAAOld");
        assert_eq!(kh.validate(&same), HostKeyDecision::Trusted);
        // 算法不同 -> 该算法下仍是 Unknown，而不是拿 ed25519 的记录去比 rsa
        let rsa = entry("db1", "ssh-rsa", "BBB");
        assert!(matches!(kh.validate(&rsa), HostKeyDecision::Unknown { .. }));
        kh.trust(same);
        assert_eq!(kh.entries().len(), 1);
    }

    #[test]
    fn trust_replaces_the_same_pattern_and_algorithm_only() {
        let mut kh = KnownHosts {
            path: PathBuf::from("x"),
            entries: vec![entry("db1", "ssh-ed25519", "old"), entry("db1", "ssh-rsa", "keep")],
        };
        kh.trust(entry("db1", "ssh-ed25519", "new"));
        assert_eq!(kh.entries().len(), 2, "换了密钥不该把别的算法一起清掉");
        assert_eq!(kh.find("db1", 22, "ssh-ed25519").unwrap().base64_key, "new");
        assert_eq!(kh.find("db1", 22, "ssh-rsa").unwrap().base64_key, "keep");
    }

    #[test]
    fn parse_skips_hashed_cert_and_comment_lines_without_failing() {
        let mut skipped = 0;
        let parsed = parse_lines(
            "# comment\n\
             db1 ssh-ed25519 AAAA\n\
             |1|abc=|def= ssh-rsa BBBB\n\
             @cert-authority *.example.com ssh-rsa CCCC\n\
             broken-line-without-key\n",
            KnownHostSource::Imported,
            &mut skipped,
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(skipped, 3);
        assert_eq!(parsed[0].source, KnownHostSource::Imported);
    }

    #[test]
    fn import_is_idempotent_and_never_touches_the_source_file() {
        let d = dir("import");
        let source = d.join("system_known_hosts");
        let original = "old-host ssh-ed25519 AAA\nnew-host ssh-ed25519 BBB\n";
        std::fs::write(&source, original).unwrap();

        let mut kh = KnownHosts {
            path: d.join("known_hosts"),
            entries: vec![entry("old-host", "ssh-ed25519", "AAA")],
        };
        assert_eq!(
            kh.import_readonly(&source).unwrap(),
            1,
            "只有 new-host 是新增，old-host 该被认成已有"
        );
        assert_eq!(kh.import_readonly(&source).unwrap(), 0, "重复导入必须幂等");
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            original,
            "系统的 known_hosts 是别人的文件，写一个字节都算越权"
        );
        assert!(kh
            .entries()
            .iter()
            .filter(|e| e.pattern == "new-host")
            .all(|e| e.source == KnownHostSource::Imported));
    }

    #[test]
    fn save_then_load_round_trips() {
        let d = dir("save");
        let kh = KnownHosts {
            path: d.join("known_hosts"),
            entries: vec![entry("[db1]:2222", "ssh-ed25519", "AAA")],
        };
        kh.save().unwrap();
        let reloaded = KnownHosts::load(&d).unwrap();
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.entries()[0].pattern, "[db1]:2222");
        assert_eq!(reloaded.find("db1", 2222, "ssh-ed25519").unwrap().base64_key, "AAA");
    }

    #[test]
    fn forget_removes_by_host_port_and_algorithm() {
        let mut kh = KnownHosts {
            path: PathBuf::from("x"),
            entries: vec![
                entry("[db1]:2222", "ssh-ed25519", "A"),
                entry("[db1]:2222", "ssh-rsa", "B"),
                entry("db1", "ssh-ed25519", "C"),
            ],
        };
        assert_eq!(kh.forget("db1", 2222, "ssh-ed25519"), 1);
        assert_eq!(kh.entries().len(), 2);
        assert!(kh.find("db1", 22, "ssh-ed25519").is_some(), "22 端口的记录不该被 2222 的删除波及");
    }

    #[test]
    fn glob_star_does_not_over_match() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("db?", "db1"));
        assert!(!glob_match("db?", "db12"));
        assert!(!glob_match("db*", "xdbc"));
    }
}
