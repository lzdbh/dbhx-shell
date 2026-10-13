use serde::{Deserialize, Serialize};
use specta::Type;

/// M0–M2 只落地两种协议；telnet/mosh/et/serial 属 M6，先不占位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Ssh,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Connecting,
    Connected,
    Closed,
    Error,
}

/// 会话 ID 由 Rust 侧分配，前端只透传。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct SessionId(pub u32);

/// 凭据引用。**类型层面就不允许明文出现** —— 密码/私钥永远不过 IPC。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AuthRef {
    /// 使用 vault 中该主机已存的凭据
    Host { host_id: String },
    /// 使用指定身份（密钥）+ 其关联口令
    Identity { host_id: String, identity_id: String },
    /// 走系统 ssh-agent
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Endpoint {
    pub host: String,
    /// None 表示协议默认端口（SSH 为 22）
    pub port: Option<u16>,
    pub username: String,
}

/// 会话的实时 endpoint。
///
/// vault 里的 host 在连上之后可能被用户编辑，任何判等都必须用会话建立时的
/// endpoint 覆盖 host 记录，否则会误判为不同目标而另开连接。port 缺失时取 22，
/// 不回落 host.port。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct LiveEndpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl LiveEndpoint {
    pub fn from_endpoint(endpoint: &Endpoint) -> Self {
        Self {
            host: endpoint.host.clone(),
            port: endpoint.port.unwrap_or(22),
            username: endpoint.username.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// 统一的会话建立参数：本地 PTY 与 SSH 共用一个入口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConnectSpec {
    pub protocol: Protocol,
    /// Local 协议为 None
    pub endpoint: Option<Endpoint>,
    /// Local 协议为 None；Agent 之外一律是引用
    pub auth: Option<AuthRef>,
    pub pty: PtySize,
    /// Local：指定 shell，None 用系统默认
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub term: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SessionInfo {
    pub id: SessionId,
    pub protocol: Protocol,
    pub status: SessionStatus,
    /// 主机友好名，供标签页标题
    pub label: String,
    pub live_endpoint: Option<LiveEndpoint>,
    pub pty: PtySize,
    pub encoding: TerminalEncoding,
}

/// 远端非 UTF-8 输出时的解码方式（内网老机器常见 GB18030）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEncoding {
    #[default]
    Utf8,
    Gb18030,
}

/// D1 用来验证「命令入参 + 结构体返回 + 类型生成」闭环。
///
/// `u64` 能出现在契约里，是因为 `dbhx-ipc` 的 builder 开了
/// `dangerously_cast_bigints_to_number()`：TS 侧统一看成 `number`。
/// 前提是值始终 < 2^53 ≈ 9.0e15——epoch 毫秒（~1.7e12）和字节计数都远不到，
/// 但任何可能超过该上界的量（累计字节、纳秒时间戳）必须改走 string。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Pong {
    pub seq: u32,
    pub server_time_ms: u64,
    pub echo: String,
    pub target: LiveEndpoint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_endpoint_defaults_missing_port_to_22_not_host_port() {
        let ep = Endpoint { host: "db1".into(), port: None, username: "root".into() };
        assert_eq!(LiveEndpoint::from_endpoint(&ep).port, 22);
    }

    #[test]
    fn auth_ref_carries_no_secret_fields() {
        let json = serde_json::to_string(&AuthRef::Identity {
            host_id: "h1".into(),
            identity_id: "k1".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"identity","host_id":"h1","identity_id":"k1"}"#);
    }

    #[test]
    fn connect_spec_tagged_enum_round_trips() {
        let spec = ConnectSpec {
            protocol: Protocol::Ssh,
            endpoint: Some(Endpoint { host: "10.0.0.1".into(), port: Some(2222), username: "app".into() }),
            auth: Some(AuthRef::Host { host_id: "h1".into() }),
            pty: PtySize::default(),
            shell: None,
            cwd: None,
            term: "xterm-256color".into(),
        };
        let back: ConnectSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(spec, back);
    }
}
