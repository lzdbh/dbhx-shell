//! 文件面板命令：开面板、列目录、建目录/建文件、删、改名、改权限，以及跟随开关。
//!
//! 「开面板」是本应用最难也最有价值的一条命令：它必须在**不重新登录**的前提下
//! 拿到一条 SFTP subsystem 通道。做法是拿终端会话的 `TransportKey` 向池要第二个
//! lease —— 命中池就不碰凭据，这一点在 `dbhx-session` 里有专门的去重与判等测试。
//!
//! 命令层持有一切异步等待之外的东西：`Mutex<Panels>` 的 guard 一律在 `await` 之前释放，
//! 否则一次慢速 `browse` 会把所有面板的命令钉住。

use std::sync::Arc;

use dbhx_model::{DirListing, ErrorCode, HostId, IpcError, Protocol, SessionId};
use dbhx_session::{LeaseKind, TransportKey};
use dbhx_sftp::{CwdSource, FollowGate, SftpClient};
use dbhx_sftp::path;

use crate::events::SftpFollowed;
use crate::state::{Core, Panel, PanelInfo};

/// 从终端 tab 打开面板：复用那条 transport，并尽量停在终端当前目录。
pub async fn open_for_session_impl(core: &Core, session: SessionId) -> Result<PanelInfo, IpcError> {
    let record = core.sessions.record(session).ok_or_else(|| IpcError::new(
        ErrorCode::NotFound,
        format!("会话 {} 已不存在，标签页可能已经关闭", session.0),
        false,
    ))?;
    let key = record.transport.clone().ok_or_else(|| IpcError::new(
        ErrorCode::InvalidInput,
        "本地终端与 telnet 都没有远端文件系统可看",
        false,
    ))?;
    open_panel(core, key, record.cwd.as_deref()).await
}

/// 从主机树直接打开面板：没有现成会话时自己拨一条（仍然免输密码，凭据来自 vault）。
pub async fn open_for_host_impl(core: &Core, host_id: HostId) -> Result<PanelInfo, IpcError> {
    let record = core.host_of(&host_id)?;
    // 只有 SSH 才谈得上复用 transport 开 SFTP 子系统；telnet 是裸 TCP，压根没有文件通道，
    // 放行到这里只会拿 SSH 的拨号去连一个 telnet 端口。前端已经置灰按钮，这里再兜一道。
    if record.protocol != Protocol::Ssh {
        return Err(IpcError::new(ErrorCode::InvalidInput, "只有 SSH 主机有远端文件系统", false));
    }
    let key = crate::commands::session::key_of_record(&record);
    open_panel(core, key, None).await
}

/// 面板已存在就复用，不会开第二条 subsystem 通道。
async fn open_panel(core: &Core, key: TransportKey, terminal_cwd: Option<&str>) -> Result<PanelInfo, IpcError> {
    // 开面板可能触发拨号（`acquire` 命中不了池时），锁屏期间一律拒绝；
    // 已经开着的面板的浏览/增删改走的是既有 client，不碰凭据，不在这里拦。
    core.require_unlocked()?;
    let connection = core.conn_id_of(&key);
    if let Some(info) = existing_panel(core, connection) {
        return Ok(info);
    }

    let lease = core.pool.acquire(key.clone(), LeaseKind::Sftp).await?;
    let endpoint = lease.endpoint().clone();
    let stream = lease.open_subsystem("sftp".to_string()).await?;
    let client = SftpClient::connect(stream).await?;

    // 首选终端报过的目录；它可能已经被人删掉了，那就退回登录目录。
    // 分两步写而不是 `or_else`：闭包里不能 `await`。
    let from_terminal = match terminal_cwd {
        Some(cwd) => client.canonicalize(cwd).await.ok(),
        None => None,
    };
    let raw_start = match from_terminal {
        Some(p) => Some(p),
        None => client.canonicalize(".").await.ok(),
    };
    let start = raw_start
        .map(|p| path::normalize(&p))
        .ok_or_else(|| IpcError::new(ErrorCode::Unreachable, "远端目录解析失败，请检查 sftp subsystem 是否可用", true))?;

    let listing = client.browse(&start).await?;

    let mut gate = FollowGate::new(connection);
    // 把终端已经报过的目录记进账本：下一次 OSC 7 只有比它更强或不同才会触发跳转
    {
        let blacklist = core.blacklist.lock().unwrap_or_else(|p| p.into_inner());
        let generation = gate.generation();
        gate.report(generation, terminal_cwd, CwdSource::Osc7, &blacklist);
    }

    core.panels.lock().unwrap_or_else(|p| p.into_inner())
        .insert(Panel { connection, client: client.clone(), lease, gate, shown: listing.path.clone() });

    Ok(PanelInfo { connection, endpoint, listing, following: true })
}

/// 已经开着的面板：直接把当前展示的状态回给前端，不开新通道。
fn existing_panel(core: &Core, connection: u32) -> Option<PanelInfo> {
    let panels = core.panels.lock().unwrap_or_else(|p| p.into_inner());
    let panel = panels.get(connection)?;
    Some(PanelInfo {
        connection,
        endpoint: panel.lease.endpoint().clone(),
        listing: DirListing { path: panel.shown.clone(), entries: vec![] },
        following: panel.gate.is_following(),
    })
}

/// 列目录。`target` 为 `None` 时刷新当前目录；是相对路径时相对当前目录解析。
/// 用户点击导致的导航会记进门控，之后终端的旧目录回声不会把面板拽回去。
pub async fn browse_impl(core: &Core, connection: u32, target: Option<String>) -> Result<DirListing, IpcError> {
    let (client, dest) = {
        let panels = core.panels.lock().unwrap_or_else(|p| p.into_inner());
        let panel = panels.get(connection).ok_or_else(|| crate::state::not_found_panel(connection))?;
        let dest = match target {
            Some(t) => path::join(&panel.shown, &t),
            None => panel.shown.clone(),
        };
        (panel.client.clone(), dest)
    };
    let listing = client.browse(&dest).await?;
    let mut panels = core.panels.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(panel) = panels.get_mut(connection) {
        panel.shown = listing.path.clone();
        panel.gate.user_browsed(&listing.path);
    }
    Ok(listing)
}

pub async fn mkdir_impl(core: &Core, connection: u32, target: String, recursive: bool) -> Result<(), IpcError> {
    // 裸名字按面板正在显示的目录拼成绝对路径再发出去。直接丢给服务器的话，它用的是
    // 会话初始目录（root 用户即 `/root`），用户就会在看着 A 目录的时候把东西建到 B 去。
    let (client, shown) = core.panel_at(connection)?;
    let dest = path::join(&shown, &target);
    if recursive { client.mkdir_all(&dest).await? } else { client.mkdir(&dest).await? };
    Ok(())
}

pub async fn create_file_impl(core: &Core, connection: u32, target: String) -> Result<(), IpcError> {
    // 与 mkdir 同一口径：裸名字按面板正在显示的目录拼，否则会建到会话初始目录去。
    let (client, shown) = core.panel_at(connection)?;
    client.create_file(&path::join(&shown, &target)).await.map_err(Into::into)
}

/// 改远端权限位。`mode` 是 `0o000..=0o7777` 的整数，越界直接拒——悄悄取模会让用户
/// 以为改完了，实际服务端收到的是另一个值。
pub async fn chmod_impl(core: &Core, connection: u32, target: String, mode: u32) -> Result<(), IpcError> {
    if mode > 0o7777 {
        return Err(IpcError::new(ErrorCode::InvalidInput, "权限位只能是 0 到 7777 的八进制数", false));
    }
    let (client, _) = core.panel_at(connection)?;
    client.set_mode(&target, mode).await.map_err(Into::into)
}

pub async fn remove_impl(core: &Core, connection: u32, target: String, is_dir: bool) -> Result<(), IpcError> {
    let client = core.panel_client(connection)?;
    if is_dir { client.remove_dir(&target).await? } else { client.remove_file(&target).await? };
    Ok(())
}

pub async fn rename_impl(core: &Core, connection: u32, from: String, to: String, overwrite: bool) -> Result<(), IpcError> {
    let (client, _) = core.panel_at(connection)?;
    // 目标名按 `from` **自己所在的目录**拼，而不是按面板显示的目录：改名就该是原地的
    // 事。两者正常情况下是同一个，但万一列表是旧的、面板已经跳走，宁可报错也不要把
    // 文件搬到别处去。`to` 给绝对路径时 `join` 原样返回，跨目录移动仍然是显式能力。
    let dest = match path::parent(&from) {
        Some(dir) => path::join(&dir, &to),
        None => to.clone(),
    };
    client.rename(&from, &dest, overwrite).await.map_err(Into::into)
}

/// 关面板。lease 随 `Panel` 一起释放，连接是否断开交给池的 TTL 决定。
///
/// 有传输在跑的面板关不掉：批自己持着会话引用，面板没了它也不会停，用户只会看到
/// 进度条凭空消失。要先 `transfer_cancel`。
pub async fn close_impl(core: &Core, connection: u32) -> Result<(), IpcError> {
    if core.panels_with_batch().contains(&connection) {
        return Err(IpcError::new(
            ErrorCode::InvalidInput,
            "该面板有传输正在进行，请先取消再关闭",
            false,
        ));
    }
    let Some(panel) = core.remove_panel(connection) else {
        return Err(crate::state::not_found_panel(connection));
    };
    // 通道自己先关掉，免得服务端那边还挂着一个空 subsystem 会话
    let _ = panel.client.close().await;
    core.clear_blacklist(connection);
    core.forget_conn(connection);
    Ok(())
}

/// 用户锁定了面板（自己在远端浏览，不想被终端拽走）。
/// 重新打开跟随时若终端目录有变化，门控会立刻给出该跳哪儿。
pub async fn set_following_impl(core: &Core, connection: u32, following: bool) -> Result<SftpFollowed, IpcError> {
    let decision = core.decide(connection, |gate, blacklist| gate.set_following(following, blacklist))?;
    Ok(core.apply_decision(connection, decision).await)
}

/// 正在改名/新建：这段时间不许跳目录，否则用户填一半路径面板先跑了。
/// 失焦时门控会补上期间被压住的那次跳转。
pub async fn set_editing_impl(core: &Core, connection: u32, editing: bool) -> Result<SftpFollowed, IpcError> {
    let decision = core.decide(connection, |gate, blacklist| gate.set_editing(editing, blacklist))?;
    Ok(core.apply_decision(connection, decision).await)
}

type State<'a> = tauri::State<'a, Arc<Core>>;

#[tauri::command]
#[specta::specta]
pub async fn sftp_open_for_session(core: State<'_>, session: SessionId) -> Result<PanelInfo, IpcError> {
    open_for_session_impl(&core, session).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_open_for_host(core: State<'_>, host_id: HostId) -> Result<PanelInfo, IpcError> {
    open_for_host_impl(&core, host_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_browse(core: State<'_>, connection: u32, path: Option<String>) -> Result<DirListing, IpcError> {
    browse_impl(&core, connection, path).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_mkdir(core: State<'_>, connection: u32, path: String, recursive: bool) -> Result<(), IpcError> {
    mkdir_impl(&core, connection, path, recursive).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_create_file(core: State<'_>, connection: u32, path: String) -> Result<(), IpcError> {
    create_file_impl(&core, connection, path).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_chmod(core: State<'_>, connection: u32, path: String, mode: u32) -> Result<(), IpcError> {
    chmod_impl(&core, connection, path, mode).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_remove(core: State<'_>, connection: u32, path: String, is_dir: bool) -> Result<(), IpcError> {
    remove_impl(&core, connection, path, is_dir).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_rename(core: State<'_>, connection: u32, from: String, to: String, overwrite: bool) -> Result<(), IpcError> {
    rename_impl(&core, connection, from, to, overwrite).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_close(core: State<'_>, connection: u32) -> Result<(), IpcError> {
    close_impl(&core, connection).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_set_following(core: State<'_>, connection: u32, following: bool) -> Result<SftpFollowed, IpcError> {
    set_following_impl(&core, connection, following).await
}

#[tauri::command]
#[specta::specta]
pub async fn sftp_set_editing(core: State<'_>, connection: u32, editing: bool) -> Result<SftpFollowed, IpcError> {
    set_editing_impl(&core, connection, editing).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_local_session_has_no_remote_filesystem() {
        // 「本地终端不能开面板」必须是明确的错误，而不是 browse 时莫名超时
        let (core, _rx) = crate::state::tests_harness("local-panel", crate::state::fake_dialer());
        let info = crate::commands::session::open_local_impl(
            &core,
            crate::commands::session::default_local_spec(None, dbhx_model::PtySize::default()),
            "本地".into(),
        )
        .await
        .unwrap();
        let err = open_for_session_impl(&core, info.id).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("本地终端"), "{}", err.message);
    }

    #[tokio::test]
    async fn browsing_a_panel_that_was_never_opened_is_not_retryable() {
        let (core, _rx) = crate::state::tests_harness("no-panel", crate::state::fake_dialer());
        let err = browse_impl(&core, 42, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("42"), "{}", err.message);
        assert!(!err.retryable, "面板没打开过，重试一百次也不会自己开起来");
    }

    #[tokio::test]
    async fn closing_a_panel_twice_reports_the_same_shape_as_a_missing_one() {
        let (core, _rx) = crate::state::tests_harness("close-twice", crate::state::fake_dialer());
        let first = close_impl(&core, 7).await.unwrap_err();
        let second = close_impl(&core, 7).await.unwrap_err();
        assert_eq!(first.code, second.code);
    }

    #[tokio::test]
    async fn opening_a_panel_for_a_deleted_host_fails_cleanly() {
        let (core, _rx) = crate::state::tests_harness("gone-host", crate::state::fake_dialer());
        let err = open_for_host_impl(&core, HostId("nope".into())).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert_eq!(core.panels.lock().unwrap().len(), 0, "失败不能留下半个面板");
    }

    #[tokio::test]
    async fn dialing_a_reusable_connection_still_needs_the_sftp_subsystem() {
        // 假拨号器直接失败，所以这里断言的是「走到池」而不是「拿到了通道」
        let (core, _rx) = crate::state::tests_harness("dial-fail", crate::state::fake_dialer());
        let err = open_for_host_impl(&core, add_host(&core, "db9")).await.unwrap_err();
        assert!(matches!(err.code, ErrorCode::ChannelClosed | ErrorCode::Unreachable), "{:?}", err.code);
        assert_eq!(core.panels.lock().unwrap().len(), 0);
        assert_eq!(core.conn_id_count(), 1, "拨号失败也要留号：同一主机重试不该换号");
    }

    fn add_host(core: &Arc<Core>, host: &str) -> HostId {
        crate::commands::vault::upsert_host_impl(
            core,
            crate::commands::vault::HostDraft {
                id: None,
                label: host.into(),
                group: None,
                tags: vec![],
                favorite: false,
                protocol: dbhx_model::Protocol::Ssh,
                os: dbhx_model::OsType::Linux,
                endpoint: dbhx_model::Endpoint { host: host.into(), port: Some(22), username: "app".into() },
                auth_mode: dbhx_model::AuthMode::Agent,
                identity_id: None,
                keepalive_secs: 0,
                encoding: dbhx_model::TerminalEncoding::Utf8,
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_telnet_host_has_no_file_panel_and_is_refused_before_dialing() {
        // 后端兜底：telnet 没有 SFTP 子系统，绝不能在拨号那一刻才发现
        let (core, _rx) = crate::state::tests_harness("sftp-telnet", crate::state::fake_dialer());
        let id = crate::commands::vault::upsert_host_impl(
            &core,
            crate::commands::vault::HostDraft {
                id: None,
                label: "老设备".into(),
                group: None,
                tags: vec![],
                favorite: false,
                protocol: dbhx_model::Protocol::Telnet,
                os: dbhx_model::OsType::Other,
                endpoint: dbhx_model::Endpoint { host: "10.0.0.5".into(), port: Some(23), username: String::new() },
                auth_mode: dbhx_model::AuthMode::Agent,
                identity_id: None,
                keepalive_secs: 0,
                encoding: dbhx_model::TerminalEncoding::Utf8,
            },
        )
        .unwrap();
        let err = open_for_host_impl(&core, id).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert_eq!(core.conn_id_count(), 0, "被协议挡在门外时压根不该起一条连接号");
    }
}
