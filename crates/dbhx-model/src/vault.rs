//! Vault 领域的纯类型：主机、身份、密钥引用。
//!
//! 边界规则：这些类型里**永远没有明文凭据**。凭据是 `SecretId`，
//! 真实字节只存在于 `dbhx-vault` 的加密文档里，跨 IPC 的只有 id。
//! 需要临时用一下明文时，回调参数是 `Zeroizing<[u8]>`，出了闭包即擦除。

use serde::{Deserialize, Serialize};
use specta::Type;

/// vault 内部 id（uuid4 的 hex，由 vault 生成，前端只透传）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct HostId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct IdentityId(pub String);

/// 密文条目的引用。它本身不是秘密，但拿着它能向 vault 换出秘密，
/// 所以只允许在进程内传递，不写日志、不进 URL。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct SecretId(pub String);

/// 认证方式。`Agent` 与 `KeyboardInteractive` 不需要 vault 里存任何凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Password,
    PublicKey,
    Agent,
    /// 堡垒机常见：口令之外还要交互问答，走 SSH 的 keyboard-interactive
    KeyboardInteractive,
}

/// 侧栏分组方式，纯 UI 语义，但存进 vault 才能跨重启保持。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum GroupMode {
    #[default]
    Group,
    Tag,
    Favorite,
}

/// 主机那边跑的是什么系统，细到发行版。纯展示语义（决定列表里那颗图标），不参与拨号。
///
/// 为什么要分发行版而不是只留「Linux」一档：这一列的价值全在「扫一眼认出哪台是哪台」，
/// 而运维眼里 Ubuntu 和 CentOS 是两种机器。分得太粗，图标就退化成一行装饰。
///
/// 默认 `Unknown`：用户还没告诉我们，而不是「探测失败」。对图标来说两者没区别——
/// 都画通用图形，不去猜一个看起来像依据的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum OsType {
    #[default]
    Unknown,
    /// 只知道是 Linux，发行版不明
    Linux,
    Ubuntu,
    Debian,
    Centos,
    /// CentOS 的两条继任线，国内内网正在陆续换过去
    Rocky,
    Alma,
    /// Red Hat Enterprise Linux。变体名不用 `Redhat`：读的人得反应一下那是不是个词。
    Rhel,
    Fedora,
    Opensuse,
    Arch,
    Alpine,
    Kali,
    /// 深度 / 统信 UOS 那一支，政企内网里常见
    Deepin,
    Freebsd,
    Windows,
    /// macOS。变体名刻意不叫 `MacOS`：大小写混排序列化成 snake_case 会变成 `mac_os`。
    Mac,
    /// 明确不是上面任何一种（交换机、路由器、自研设备…）
    Other,
}

/// 一台主机。`endpoint` 是「拨号用什么」，`label` 是「人看什么」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct HostRecord {
    pub id: HostId,
    pub label: String,
    /// 侧栏分组名，None 表示未分组
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub favorite: bool,
    pub protocol: crate::session::Protocol,
    /// `#[serde(default)]` 是给存量 vault 文件留的：加这个字段之前建的机器没有它，
    /// 缺省解析成 `Unknown`，而不是整条记录读不出来。
    #[serde(default)]
    pub os: OsType,
    pub endpoint: crate::session::Endpoint,
    pub auth_mode: AuthMode,
    /// `auth_mode` 为 PublicKey 时指向一把身份
    pub identity_id: Option<IdentityId>,
    /// `auth_mode` 为 Password / KeyboardInteractive 时指向一条口令密文
    pub password_secret_id: Option<SecretId>,
    /// 0 表示不发 keepalive；秒数，远端掉线检测用
    pub keepalive_secs: u16,
    pub encoding: crate::session::TerminalEncoding,
    /// epoch 毫秒。见 `Pong::server_time_ms` 的说明：按 number 导出是安全的
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// 一把私钥（+可选口令）。私钥与口令各自是独立密文条目，
/// 这样同一把口令可以属于多个身份，删除身份时按引用计数清理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct IdentityRecord {
    pub id: IdentityId,
    pub label: String,
    pub private_key_secret_id: SecretId,
    pub passphrase_secret_id: Option<SecretId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// 给前端看的主机投影。字段与 `HostRecord` 同源，但 `auth_ref` 已经把
/// 「用哪个凭据」压成会话层真正需要的形式，省得前端自己拼。
///
/// `auth_mode` 与 `auth` 看着重复，其实不然：`AuthRef::Host` 同时覆盖
/// `Password` 与 `KeyboardInteractive` 两种模式，只给 `auth` 的话，编辑表单
/// 无法还原用户当初选的是哪种，保存时会把它悄悄降级成密码认证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct HostInfo {
    pub id: HostId,
    pub label: String,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub favorite: bool,
    pub protocol: crate::session::Protocol,
    /// 列表图标用，见 `OsType`
    pub os: OsType,
    pub endpoint: crate::session::Endpoint,
    pub auth_mode: AuthMode,
    pub auth: crate::session::AuthRef,
    /// vault 里是否已存凭据（口令）。纯布尔存在性标记，不带引用、不含任何凭据名，
    /// 供编辑表单决定「查看口令」按钮显不显示。命名刻意避开 password/secret 等词，
    /// 这样 `host_projection_never_carries_secret_material` 的按名守卫可以继续一刀切。
    pub has_credential: bool,
    pub keepalive_secs: u16,
    pub encoding: crate::session::TerminalEncoding,
}

impl HostInfo {
    pub fn from_record(record: &HostRecord) -> Self {
        let auth = match record.auth_mode {
            AuthMode::Agent => crate::session::AuthRef::Agent,
            AuthMode::Password | AuthMode::KeyboardInteractive => crate::session::AuthRef::Host {
                host_id: record.id.0.clone(),
            },
            AuthMode::PublicKey => crate::session::AuthRef::Identity {
                host_id: record.id.0.clone(),
                identity_id: record
                    .identity_id
                    .as_ref()
                    .map(|i| i.0.clone())
                    .unwrap_or_default(),
            },
        };
        Self {
            id: record.id.clone(),
            label: record.label.clone(),
            group: record.group.clone(),
            tags: record.tags.clone(),
            favorite: record.favorite,
            protocol: record.protocol,
            os: record.os,
            endpoint: record.endpoint.clone(),
            auth_mode: record.auth_mode,
            auth,
            has_credential: record.password_secret_id.is_some(),
            keepalive_secs: record.keepalive_secs,
            encoding: record.encoding,
        }
    }
}

/// 主机密钥记录。OpenSSH `known_hosts` 的一行，明文存（完整性敏感但不机密）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KnownHost {
    /// `host:port` 或 `[host]:port`，与 OpenSSH 一致；通配模式按字面存
    pub pattern: String,
    /// 例如 `ssh-ed25519`
    pub algorithm: String,
    /// base64 编码的 wire-format 公钥，不含算法前缀
    pub base64_key: String,
    /// 首次确认时间；被 `Imported` 时说明来源是系统 known_hosts
    pub added_at_ms: u64,
    pub source: KnownHostSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum KnownHostSource {
    /// 用户在 App 里逐次确认加的
    UserConfirmed,
    /// 从系统 `~/.ssh/known_hosts` 只读导入的，App 不会替用户改原件
    Imported,
}

/// 主机密钥校验结论。`Changed` 与 `Unknown` 的区别是硬性的：
/// 前者 UI 必须阻断且不给「仍然连接」按钮，后者可以确认放行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HostKeyDecision {
    /// 与已存记录一致
    Trusted,
    Unknown {
        pending: Box<KnownHost>,
    },
    Changed {
        stored: Box<KnownHost>,
        presented: Box<KnownHost>,
    },
}

/// 云同步落在哪家「片段」服务上。
///
/// 之所以放在这一层而不是 `dbhx-sync`：它要进 vault 文档持久化，又要在 IPC 上走到
/// 设置面板，而 `dbhx-vault` 不能反向依赖 `dbhx-sync`（那边已经依赖它）。
///
/// 每加一家都要先实测它的字段形状：GitHub 与 Gitee 在「创建用 `files` 哈希、
/// 更新却用平铺 `filename`+`content`」这类细节上并不一致，靠猜写出来的解析
/// 只会以「云端响应读不懂」的形式在用户那边失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// GitHub Gist。**实测 `public:false` 也是匿名可读的**，所以云端那份密文要按
    /// 「谁都能下载、可以无限次离线爆破」设防，见 `dbhx-vault::envelope`。
    GitHub,
}

/// 云端那一份的坐标：哪家服务 + 哪个片段。
///
/// 它**不是**秘密，可以明文存在 vault 里：片段 id 本身就是访问控制的一部分，
/// 真正的秘密只有同步串，而那串是按密文条目存的。
/// 之所以要存：`create` 只在第一次推送时跑一次，之后必须原地 `update`，
/// 否则每同步一次云上就多一份，第二台设备就不知道读哪一份了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SyncTarget {
    pub provider: ProviderKind,
    /// 片段 id，取创建响应里的 `id` 字段。它会拼进请求地址，而令牌放在 header 里，
    /// 所以 `dbhx-sync` 在发请求前过一道字符白名单：用户从网页地址栏粘来的东西
    /// 不该有机会改变请求要去哪台机器。
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Endpoint, Protocol, TerminalEncoding};

    fn host(mode: AuthMode, identity: Option<&str>) -> HostRecord {
        HostRecord {
            id: HostId("h1".into()),
            label: "生产库".into(),
            group: Some("db".into()),
            tags: vec!["prod".into()],
            favorite: true,
            protocol: Protocol::Ssh,
            os: OsType::Linux,
            endpoint: Endpoint { host: "10.0.0.1".into(), port: Some(2222), username: "app".into() },
            auth_mode: mode,
            identity_id: identity.map(|i| IdentityId(i.into())),
            password_secret_id: None,
            keepalive_secs: 30,
            encoding: TerminalEncoding::Utf8,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    #[test]
    fn host_projection_never_carries_secret_material() {
        let json = serde_json::to_string(&HostInfo::from_record(&host(AuthMode::Password, None))).unwrap();
        // 只允许出现 secret 的「引用」，不允许出现任何像是凭据的字段。
        // 按**字段名**判断而不是按整串文本 grep：`auth_mode: "password"` 是模式名，
        // 而 `password_secret_id` 才是引用，只有后者必须消失。
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys = key_paths(&value);
        assert!(keys.iter().any(|k| k.ends_with("host_id")), "{keys:?}");
        for forbidden in ["password", "passphrase", "private_key", "secret"] {
            let hit = keys.iter().find(|k| k.rsplit('.').next().unwrap_or_default().contains(forbidden));
            assert!(hit.is_none(), "HostInfo 泄露了字段 `{hit:?}`: {json}");
        }
    }

    /// 前端的图标表（`OS_GLYPH`）是按这些名字取值的，改拼法等于悄悄丢图标，
    /// 所以把名字钉在这里：想改就得同时改前端那张表。
    #[test]
    fn os_type_json_names_are_frozen() {
        for (os, want) in [
            (OsType::Unknown, "\"unknown\""),
            (OsType::Linux, "\"linux\""),
            (OsType::Rocky, "\"rocky\""),
            (OsType::Alma, "\"alma\""),
            (OsType::Rhel, "\"rhel\""),
            (OsType::Opensuse, "\"opensuse\""),
            (OsType::Deepin, "\"deepin\""),
            (OsType::Freebsd, "\"freebsd\""),
            (OsType::Mac, "\"mac\""),
        ] {
            assert_eq!(serde_json::to_string(&os).unwrap(), want);
        }
    }

    /// 递归收集对象字段名，形如 `auth.host_id`。
    fn key_paths(value: &serde_json::Value) -> Vec<String> {
        fn walk(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                        out.push(path.clone());
                        walk(child, &path, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, prefix, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(value, "", &mut out);
        out
    }

    #[test]
    fn provider_kind_json_name_is_frozen_because_it_is_stored_and_sent() {
        // 这个字符串同时出现在 vault 文档和 IPC 契约里：改拼法等于让老库里的
        // `sync_target` 悄悄读不出来，而报错长得像「没配过同步」。
        assert_eq!(serde_json::to_string(&ProviderKind::GitHub).unwrap(), "\"github\"");
        let target = SyncTarget { provider: ProviderKind::GitHub, id: "abc123".into() };
        assert_eq!(
            serde_json::from_str::<SyncTarget>(&serde_json::to_string(&target).unwrap()).unwrap(),
            target
        );
    }

    #[test]
    fn public_key_hosts_project_to_identity_auth_ref() {
        let info = HostInfo::from_record(&host(AuthMode::PublicKey, Some("k1")));
        assert_eq!(
            info.auth,
            crate::session::AuthRef::Identity { host_id: "h1".into(), identity_id: "k1".into() }
        );
    }

    #[test]
    fn host_key_decision_distinguishes_unknown_from_changed() {
        let stored = KnownHost {
            pattern: "[10.0.0.1]:2222".into(),
            algorithm: "ssh-ed25519".into(),
            base64_key: "AAAAC3".into(),
            added_at_ms: 1,
            source: KnownHostSource::UserConfirmed,
        };
        let other = KnownHost { base64_key: "BBBB".into(), ..stored.clone() };
        let changed = HostKeyDecision::Changed {
            stored: Box::new(stored.clone()),
            presented: Box::new(other),
        };
        assert_ne!(changed, HostKeyDecision::Unknown { pending: Box::new(stored) });
        let json = serde_json::to_string(&changed).unwrap();
        assert!(json.starts_with(r#"{"kind":"changed""#), "{json}");
    }
}
