//! 会话命令：打开、输入、改尺寸、关闭、列举。
//!
//! 每个命令都是「取参数 → 调下层 → 翻译错误」三行，业务规则一律在下层 crate。
//! 拆成 `*_impl`（拿 `&Core`）与 `#[tauri::command]`（拿 `State`）两层，
//! 是为了让逻辑测试不需要 Tauri 运行时：测试直接调 `*_impl`。

use dbhx_model::{
    ConnectSpec, ErrorCode, HostId, IpcError, LiveEndpoint, Protocol, PtySize, SessionId, SessionInfo,
    TerminalEncoding,
};
use dbhx_pty::LocalSpec;
use dbhx_session::TransportKey;

use crate::state::Core;

/// 终端类型。M4 会把它挪进设置里，现在固定一个兼容性最好的值。
const DEFAULT_TERM: &str = "xterm-256color";

pub async fn open_local_impl(core: &Core, spec: ConnectSpec, label: String) -> Result<SessionInfo, IpcError> {
    if spec.protocol != Protocol::Local {
        return Err(IpcError::new(ErrorCode::InvalidInput, "本地终端的 protocol 必须是 local", false));
    }
    let record = core.sessions.open_local(&LocalSpec::from_connect(&spec), label, TerminalEncoding::default())?;
    Ok(record.info())
}

/// 开一个远端 shell。凭据只在拨号内部出现，这一层拿到的一直是引用。
pub async fn open_remote_impl(core: &Core, host_id: HostId, pty: PtySize) -> Result<SessionInfo, IpcError> {
    // 锁屏期间禁止新拨号：这是意图层的拦截，`build_plan` 里还有一道以原子值为准的兜底。
    core.require_unlocked()?;
    let record = core.host_of(&host_id)?;
    let label = record.label.clone();
    let encoding = record.encoding;
    let key = key_of_record(&record);
    let session = core.sessions.open_remote(key, DEFAULT_TERM.to_string(), pty, encoding, label).await?;
    Ok(session.info())
}

/// vault 记录 ⇒ 会话身份键。
///
/// 面板要复用终端那条 transport 时，两边的键必须同构，所以这个函数是 `pub`，
/// 而且只有一个入参——少一个入参就少一处可能算错的地方。
pub fn key_of_record(record: &dbhx_model::HostRecord) -> TransportKey {
    let auth = dbhx_model::HostInfo::from_record(record).auth;
    TransportKey::new(record.id.clone(), LiveEndpoint::from_endpoint(&record.endpoint), auth, record.keepalive_secs)
}

pub async fn write_impl(core: &Core, session: SessionId, bytes: Vec<u8>) -> Result<(), IpcError> {
    core.sessions.write(session, &bytes).await.map_err(Into::into)
}

pub async fn resize_impl(core: &Core, session: SessionId, pty: PtySize) -> Result<(), IpcError> {
    core.sessions.resize(session, &pty).await.map_err(Into::into)
}

/// 关闭 tab。连接不会因此断开——idle park 归池管，见 `dbhx-session`。
pub async fn close_impl(core: &Core, session: SessionId) -> Result<(), IpcError> {
    core.sessions.close(session).await.map_err(Into::into)
}

pub fn list_impl(core: &Core) -> Vec<SessionInfo> {
    core.sessions.list().iter().map(|r| r.info()).collect()
}

/// 这个 tab 用的是哪条 transport。`None` 表示本地终端或 tab 已关闭。
pub fn transport_of(core: &Core, session: SessionId) -> Option<TransportKey> {
    core.sessions.transport_key(session)
}

/// 本地终端的默认参数：前端不该知道本机 shell 叫什么，交给 portable-pty 挑。
pub fn default_local_spec(cwd: Option<String>, pty: PtySize) -> ConnectSpec {
    ConnectSpec {
        protocol: Protocol::Local,
        endpoint: None,
        auth: None,
        pty,
        shell: None,
        cwd,
        term: DEFAULT_TERM.to_string(),
    }
}

type State<'a> = tauri::State<'a, std::sync::Arc<Core>>;

#[tauri::command]
#[specta::specta]
pub async fn session_open_local(
    core: State<'_>,
    spec: ConnectSpec,
    label: String,
) -> Result<SessionInfo, IpcError> {
    open_local_impl(&core, spec, label).await
}

#[tauri::command]
#[specta::specta]
pub async fn session_open_remote(core: State<'_>, host_id: HostId, pty: PtySize) -> Result<SessionInfo, IpcError> {
    open_remote_impl(&core, host_id, pty).await
}

/// 键盘输入。方向与输出相反：这里用 `Vec<u8>` 而不是 base64。
/// 输入端每次只有几个字节，serde_json 的数字数组那点浪费可以忽略，
/// 而少一处编解码就少一处出错的可能。
#[tauri::command]
#[specta::specta]
pub async fn session_write(core: State<'_>, session: SessionId, bytes: Vec<u8>) -> Result<(), IpcError> {
    write_impl(&core, session, bytes).await
}

#[tauri::command]
#[specta::specta]
pub async fn session_resize(core: State<'_>, session: SessionId, pty: PtySize) -> Result<(), IpcError> {
    resize_impl(&core, session, pty).await
}

#[tauri::command]
#[specta::specta]
pub async fn session_close(core: State<'_>, session: SessionId) -> Result<(), IpcError> {
    close_impl(&core, session).await
}

#[tauri::command]
#[specta::specta]
pub fn session_list(core: State<'_>) -> Result<Vec<SessionInfo>, IpcError> {
    Ok(list_impl(&core))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::{AuthMode, Endpoint, HostRecord, Protocol};

    fn host() -> HostRecord {
        HostRecord {
            id: HostId("h1".into()),
            label: "生产库".into(),
            group: None,
            tags: vec![],
            favorite: false,
            protocol: Protocol::Ssh,
            endpoint: Endpoint { host: "10.0.0.1".into(), port: None, username: "app".into() },
            auth_mode: AuthMode::Agent,
            identity_id: None,
            password_secret_id: None,
            keepalive_secs: 30,
            encoding: TerminalEncoding::Utf8,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    #[test]
    fn missing_port_becomes_22_in_the_transport_key() {
        // 面板复用终端的连接全靠这个键：端口归一化不一致会另开一条 transport，
        // 于是「免二次登录」变成「多登录一次」
        let key = key_of_record(&host());
        assert_eq!(key.endpoint.port, 22);
        assert_eq!(key.endpoint.host, "10.0.0.1");
        assert_eq!(key.auth, dbhx_model::AuthRef::Agent);
        assert_eq!(key.keepalive, Some(std::time::Duration::from_secs(30)));
    }

    #[test]
    fn local_spec_leaves_the_shell_to_the_platform() {
        let spec = default_local_spec(Some("/tmp".into()), PtySize { rows: 40, cols: 120 });
        assert_eq!(spec.protocol, Protocol::Local);
        assert_eq!(spec.term, "xterm-256color");
        let local = LocalSpec::from_connect(&spec);
        assert_eq!(local.program, None, "None 表示让 portable-pty 挑默认 shell");
        assert_eq!(local.pty.rows, 40);
    }

    #[tokio::test]
    async fn a_remote_open_for_an_unknown_host_is_not_retryable() {
        // 记录被删了以后还拿着旧 id 的 tab：重试不会凭空变出记录
        let (core, _rx) = crate::state::tests_harness("unknown-host", crate::state::fake_dialer());
        let err = open_remote_impl(&core, HostId("gone".into()), PtySize::default()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(!err.retryable, "{}", err.message);
    }

    #[tokio::test]
    async fn a_local_session_shows_up_in_the_list_and_closes_again() {
        let (core, _rx) = crate::state::tests_harness("local-list", crate::state::fake_dialer());
        let info = open_local_impl(&core, default_local_spec(None, PtySize::default()), "本地".into()).await.unwrap();
        assert_eq!(info.protocol, Protocol::Local);
        assert_eq!(info.label, "本地");
        assert_eq!(list_impl(&core).len(), 1);
        assert!(transport_of(&core, info.id).is_none(), "本地终端没有 transport");

        close_impl(&core, info.id).await.unwrap();
        // 关闭是异步收尾：进程退出、读端 EOF、条目摘除串在一条链上，这里给它一点时间
        for _ in 0..50 {
            if list_impl(&core).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(list_impl(&core).is_empty());
    }

    #[tokio::test]
    async fn writing_to_a_session_that_is_not_there_reports_the_right_code() {
        let (core, _rx) = crate::state::tests_harness("write-missing", crate::state::fake_dialer());
        let err = write_impl(&core, SessionId(999), b"ls\r".to_vec()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("999"), "{}", err.message);
    }
}
