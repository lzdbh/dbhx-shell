//! 连上之后把远端的真实系统读回来。
//!
//! 主机记录里的「系统」只服务列表图标：新建时手选过，或者一直停在「未知」。这个模块
//! 在 SSH 会话刚立起来时顺手跑一条 exec，把发行版读回来，**只在记录仍是「未知」时**
//! 落库——手选值是用户的意志，绝不覆盖；探测失败一律静默，图标值不上打扰人。
//!
//! 探测不新拨号：借会话那条刚连上的 transport（同键必然复用），不碰凭据与主机密钥。
//! 一次 exec、几毫秒、通道结束即收回，比开文件面板还轻。

use std::sync::{Arc, Mutex};

use dbhx_model::{now_ms, HostId, OsType};
use dbhx_session::{LeaseKind, TransportKey, TransportPool};
use dbhx_vault::Vault;
use tokio::sync::mpsc;

use crate::events::{BusEvent, VaultChanged};
use crate::state::Core;

/// 探测命令：一条 POSIX shell 与 Windows cmd 都能跑的短路链。
///
/// - Linux：`cat /etc/os-release` 成功，输出里有 `ID=`
/// - macOS / FreeBSD：没有 os-release，`uname -s` 给出 `Darwin` / `FreeBSD`
/// - Windows：前两条都不存在，`ver` 输出 `Microsoft Windows [Version …]`
///
/// `||` 在 cmd 里的语义与 POSIX 相同，所以一条命令覆盖三种系统。刻意不写
/// `2>/dev/null`：cmd 会把它理解成「往盘根建 `\dev\null` 文件」并真的去建，
/// 而 stderr 本来也不进解析。
pub const PROBE_COMMAND: &str = "cat /etc/os-release || uname -s || ver";

/// 解析探测输出。`None` = 没认出来——宁可不写，也不猜。
pub fn parse_probe_output(stdout: &str) -> Option<OsType> {
    match os_release_id(stdout) {
        Some(id) => Some(distro_of(&id)),
        None => system_of(stdout),
    }
}

/// 抓 os-release 的 `ID=` 行。只认行首的 `ID=`：`ID_LIKE=` 与 `VERSION_ID=` 都不能误伤。
fn os_release_id(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(value) = line.trim().strip_prefix("ID=") else { continue };
        // 老发行版（CentOS 7 那一代）给值加引号
        let value = value.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

/// 发行版 id ⇒ 图标种类。认不出的 id 落「通用 Linux」：os-release 规范只在 Linux 系
/// 落地，走到这一行就说明系统是这一族的，企鹅比地球有信息量。
fn distro_of(id: &str) -> OsType {
    match id {
        "ubuntu" => OsType::Ubuntu,
        "debian" => OsType::Debian,
        "centos" => OsType::Centos,
        "rocky" => OsType::Rocky,
        "almalinux" | "alma" => OsType::Alma,
        "rhel" => OsType::Rhel,
        "fedora" => OsType::Fedora,
        "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "sles" => OsType::Opensuse,
        "arch" => OsType::Arch,
        "alpine" => OsType::Alpine,
        "kali" => OsType::Kali,
        "deepin" | "uos" => OsType::Deepin,
        "freebsd" => OsType::Freebsd,
        _ => OsType::Linux,
    }
}

/// 没有 `ID=` 行时看 `uname -s` / `ver` 的输出。整行比对而不是抓子串：报错信息里
/// 恰好带个系统名（`bash: Linux: command not found`）不该被当成答案。Windows 例外——
/// `ver` 的输出整行是 `Microsoft Windows [Version …]`（本地化系统上版本号段会变，
/// 产品名不变），只能按包含判。
fn system_of(text: &str) -> Option<OsType> {
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line == "Darwin" {
            return Some(OsType::Mac);
        }
        if line == "FreeBSD" {
            return Some(OsType::Freebsd);
        }
        if line == "Linux" {
            return Some(OsType::Linux);
        }
        if line.contains("Windows") {
            return Some(OsType::Windows);
        }
    }
    None
}

/// 会话建立后的入口：起一个后台任务探测并落库。
///
/// 命令层拿到的是 `&Core`（活不过一次命令调用），所以把任务需要的三件东西克隆出来：
/// 池、vault、事件总线。
pub fn spawn(core: &Core, host_id: HostId, key: TransportKey) {
    let pool = Arc::clone(&core.pool);
    let vault = Arc::clone(&core.vault);
    let bus = core.bus.clone();
    tauri::async_runtime::spawn(detect_and_store(pool, vault, bus, host_id, key));
}

/// 探测 + 落库。整条链尽力而为：任何一步不成都安静收场——这只是图标。
async fn detect_and_store(
    pool: Arc<TransportPool>,
    vault: Arc<Mutex<Vault>>,
    bus: mpsc::Sender<BusEvent>,
    host_id: HostId,
    key: TransportKey,
) {
    let Some(detected) = probe(&pool, &key).await else { return };
    persist(&vault, &bus, &host_id, detected);
}

/// 一次 exec 换一个答案。失败只记调试日志。
async fn probe(pool: &Arc<TransportPool>, key: &TransportKey) -> Option<OsType> {
    let lease = match pool.acquire(key.clone(), LeaseKind::Probe).await {
        Ok(lease) => lease,
        Err(err) => {
            tracing::debug!(host = %key.describe(), ?err, "系统探测：拿不到连接，跳过");
            return None;
        }
    };
    let exec = match lease.open_exec(PROBE_COMMAND.to_string()).await {
        Ok(exec) => exec,
        Err(err) => {
            tracing::debug!(host = %key.describe(), ?err, "系统探测：exec 打不开，跳过");
            return None;
        }
    };
    let output = match exec.run_to_end().await {
        Ok(output) => output,
        Err(err) => {
            tracing::debug!(host = %key.describe(), ?err, "系统探测：输出没收全，跳过");
            return None;
        }
    };
    let detected = parse_probe_output(&output.stdout_utf8());
    tracing::debug!(host = %key.describe(), ?detected, bytes = output.stdout.len(), "系统探测完成");
    detected
}

/// 写回 vault。**判据是「当前记录仍是未知」**：探测在后台跑，这零点几秒里用户完全
/// 可能已经手选过系统，那是更高的意志；记录被删掉同理。只有真的写成了才发变更事件，
/// 否则前端会为一次没发生的修改白刷一遍。
fn persist(vault: &Arc<Mutex<Vault>>, bus: &mpsc::Sender<BusEvent>, host_id: &HostId, detected: OsType) {
    let mut vault = vault.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut record) = vault.host(host_id).cloned() else {
        tracing::debug!(host = %host_id.0, "系统探测：记录已不在，放弃写回");
        return;
    };
    if record.os != OsType::Unknown {
        tracing::debug!(host = %record.label, ?detected, "系统探测：已是手选值，不覆盖");
        return;
    }
    record.os = detected;
    record.updated_at_ms = now_ms();
    vault.upsert_host(record);
    if let Err(err) = vault.flush() {
        tracing::warn!(?err, "系统探测：落盘失败，这次结果不保留");
        return;
    }
    // 与命令层同构：计数不是秘密，前端收到就重取主机列表，图标跟着换
    let changed = VaultChanged { hosts: vault.hosts().len() as u32, secrets: vault.secret_count() as u32 };
    let _ = bus.try_send(BusEvent::VaultChanged(changed));
    tracing::info!(host = %host_id.0, ?detected, "系统探测：已按远端实际系统落库");
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_model::{AuthMode, Endpoint, HostRecord, Protocol, TerminalEncoding};

    /// 真实的 Ubuntu 24.04 输出。`ID_LIKE=debian` 是个陷阱：不能抓到它就报 Debian。
    const UBUNTU: &str = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nVERSION=\"24.04.1 LTS (Noble Numbat)\"\nID=ubuntu\nID_LIKE=debian\nHOME_URL=\"https://www.ubuntu.com/\"\n";

    #[test]
    fn an_os_release_id_line_names_the_distro() {
        assert_eq!(parse_probe_output(UBUNTU), Some(OsType::Ubuntu));
        assert_eq!(parse_probe_output("ID=debian\n"), Some(OsType::Debian));
        assert_eq!(parse_probe_output("ID=fedora\n"), Some(OsType::Fedora));
        assert_eq!(parse_probe_output("ID=arch\n"), Some(OsType::Arch));
        assert_eq!(parse_probe_output("ID=kali\n"), Some(OsType::Kali));
        assert_eq!(parse_probe_output("ID=deepin\n"), Some(OsType::Deepin));
        assert_eq!(parse_probe_output("ID=alpine\n"), Some(OsType::Alpine));
    }

    #[test]
    fn the_family_lines_cover_both_spellings() {
        // 老发行版给值加引号；继任线与国产分支各有全名/简称等写法
        assert_eq!(parse_probe_output("ID=\"centos\"\n"), Some(OsType::Centos));
        assert_eq!(parse_probe_output("ID=\"rocky\"\n"), Some(OsType::Rocky));
        assert_eq!(parse_probe_output("ID=\"almalinux\"\n"), Some(OsType::Alma));
        assert_eq!(parse_probe_output("ID=alma\n"), Some(OsType::Alma));
        assert_eq!(parse_probe_output("ID=\"rhel\"\n"), Some(OsType::Rhel));
        assert_eq!(parse_probe_output("ID=\"opensuse-leap\"\n"), Some(OsType::Opensuse));
        assert_eq!(parse_probe_output("ID=\"sles\"\n"), Some(OsType::Opensuse));
        assert_eq!(parse_probe_output("ID=uos\n"), Some(OsType::Deepin));
        assert_eq!(parse_probe_output("ID=freebsd\n"), Some(OsType::Freebsd));
    }

    #[test]
    fn version_id_and_id_like_do_not_shadow_the_id_line() {
        // 只认行首 `ID=`：`ID_LIKE` 在前、`VERSION_ID` 在后，都不能先被抓住
        let text = "ID_LIKE=\"rhel fedora\"\nVERSION_ID=\"9\"\nNAME=\"CentOS Stream\"\nID=\"centos\"\n";
        assert_eq!(parse_probe_output(text), Some(OsType::Centos));
        // 反过来：只有 ID_LIKE 没有 ID 时，不该拿 ID_LIKE 的值冒充
        assert_eq!(parse_probe_output("ID_LIKE=debian\n"), None);
    }

    #[test]
    fn an_unfamiliar_distro_is_still_linux() {
        // 认不出的发行版落通用企鹅：os-release 在，就说明是 Linux 家族，比地球有信息量
        assert_eq!(parse_probe_output("ID=manjaro\n"), Some(OsType::Linux));
        assert_eq!(parse_probe_output("ID=openwrt\n"), Some(OsType::Linux));
    }

    #[test]
    fn without_os_release_uname_names_the_system() {
        assert_eq!(parse_probe_output("Darwin\n"), Some(OsType::Mac));
        assert_eq!(parse_probe_output("FreeBSD\n"), Some(OsType::Freebsd));
        assert_eq!(parse_probe_output("Linux\n"), Some(OsType::Linux));
    }

    #[test]
    fn a_windows_ver_banner_is_recognized() {
        assert_eq!(parse_probe_output("Microsoft Windows [Version 10.0.22631.4460]\r\n"), Some(OsType::Windows));
    }

    #[test]
    fn garbage_yields_nothing_and_is_never_guessed() {
        assert_eq!(parse_probe_output(""), None);
        assert_eq!(parse_probe_output("cat: /etc/os-release: No such file or directory\n"), None);
        // 报错行里恰好带个系统名也不该被当答案：整行必须就是系统名
        assert_eq!(parse_probe_output("bash: Linux: command not found\n"), None);
    }

    fn record(os: OsType) -> HostRecord {
        HostRecord {
            id: HostId("probe-1".into()),
            label: "探测对象".into(),
            group: None,
            tags: vec![],
            favorite: false,
            protocol: Protocol::Ssh,
            os,
            endpoint: Endpoint { host: "10.0.0.9".into(), port: None, username: "app".into() },
            auth_mode: AuthMode::Password,
            identity_id: None,
            password_secret_id: None,
            keepalive_secs: 0,
            encoding: TerminalEncoding::Utf8,
            created_at_ms: 7,
            updated_at_ms: 8,
        }
    }

    /// 每个用例一份全新目录。vault 打开即落文件，用例之间不能共用一个目录。
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("dbhx-ipc-osdetect").join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_unknown_record_adopts_the_detected_os_and_announces_it() {
        let dir = scratch("adopt");
        let mut vault = Vault::open(&dir, dbhx_vault::KeySource::File(dir.join("master.key"))).unwrap();
        let host_id = vault.upsert_host(record(OsType::Unknown));
        let vault = Arc::new(Mutex::new(vault));
        let (bus, mut rx) = mpsc::channel(4);

        persist(&vault, &bus, &host_id, OsType::Ubuntu);

        assert_eq!(vault.lock().unwrap().host(&host_id).map(|r| r.os), Some(OsType::Ubuntu));
        assert!(
            matches!(rx.try_recv(), Ok(BusEvent::VaultChanged(c)) if c.hosts == 1 && c.secrets == 0),
            "落库成功必须发一次变更事件，前端才知道要换图标"
        );
        // 关掉第一份再重开：证明 flush 真的落盘了，而不是只在内存里改了改
        drop(vault);
        let reopened = Vault::open(&dir, dbhx_vault::KeySource::File(dir.join("master.key"))).unwrap();
        assert_eq!(reopened.host(&host_id).map(|r| r.os), Some(OsType::Ubuntu));
    }

    #[test]
    fn a_manual_choice_is_never_overwritten_and_nothing_is_announced() {
        let dir = scratch("manual");
        let mut vault = Vault::open(&dir, dbhx_vault::KeySource::File(dir.join("master.key"))).unwrap();
        let host_id = vault.upsert_host(record(OsType::Centos));
        let vault = Arc::new(Mutex::new(vault));
        let (bus, mut rx) = mpsc::channel(4);

        // 探测在后台跑，这零点几秒里用户手选了 CentOS：手选赢，探测结果丢
        persist(&vault, &bus, &host_id, OsType::Ubuntu);
        assert_eq!(vault.lock().unwrap().host(&host_id).map(|r| r.os), Some(OsType::Centos));
        assert!(rx.try_recv().is_err(), "没写成的探测不该发变更事件");

        // 记录在探测期间被删除：同样安静收场
        persist(&vault, &bus, &HostId("gone".into()), OsType::Ubuntu);
        assert!(rx.try_recv().is_err());
    }
}
