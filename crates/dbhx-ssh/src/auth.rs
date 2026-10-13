//! 认证流程。
//!
//! 三条路径都有真实的坑，不是「调一下 API」那么简单：
//! * 密码：不少服务器（尤其开了 PAM 的）**只 advertise `keyboard-interactive`**，
//!   直接 `authenticate_password` 会被拒；所以失败后必须自动降级一次。
//! * 密钥：RSA 要选 hash 算法。`ssh-rsa`(SHA-1) 现代 OpenSSH 默认拒绝，
//!   得先问服务器支持什么（`best_supported_rsa_hash`），问不到才退回 legacy。
//! * agent：Windows 上是 Pageant，Unix 上是 `SSH_AUTH_SOCK`；两个都要试。
//!
//! 明文口令只在这个函数内部存在，参数是 `Zeroizing`，出了作用域即归零。

use std::sync::Arc;

use russh::client::{Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::client::AuthResult;
use russh::{MethodKind, MethodSet};
use zeroize::Zeroizing;

use crate::error::SshError;
use crate::handler::ClientHandler;

/// 认证方式的抽象。明文一律用 `Zeroizing` 承载，跨 crate 边界只剩引用。
///
/// 刻意不 `derive(Debug/Clone)`：这个 enum 的调试输出与复制就是密码泄漏的常见入口。
pub enum Credential {
    Password(Zeroizing<String>),
    PrivateKey {
        /// 私钥的 PEM 文本。是**密文原样**（可能是加密的），不是解析后的对象。
        pem: Zeroizing<String>,
        passphrase: Option<Zeroizing<String>>,
        /// PuTTY 的 `.ppk` 格式
        is_ppk: bool,
    },
    /// 交给系统 ssh-agent / Pageant
    Agent,
}

/// 最多陪服务器走几轮 `keyboard-interactive` 问答，防止对方用空 prompt 把客户端挂死。
const MAX_KBI_ROUNDS: usize = 8;

pub(crate) async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    credential: Credential,
) -> Result<(), SshError> {
    let result = match credential {
        Credential::Password(password) => {
            password_flow(handle, username, &password).await?
        }
        Credential::PrivateKey { pem, passphrase, is_ppk } => {
            let key = load_private_key(&pem, passphrase.as_ref(), is_ppk)?;
            publickey_flow(handle, username, key).await?
        }
        Credential::Agent => agent_flow(handle, username).await?,
    };
    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure { partial_success, .. } if partial_success => {
            // 服务器说「这一步过了但还要继续」，M1 没有第二因子能力，明确报出来
            Err(SshError::InteractiveAuthRequired)
        }
        AuthResult::Failure { .. } => Err(SshError::AuthFailed),
    }
}

/// 密码 → 失败且服务器建议 keyboard-interactive → 用同一密码再走一次。
async fn password_flow(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    password: &Zeroizing<String>,
) -> Result<AuthResult, SshError> {
    let first = handle.authenticate_password(username, password.as_str()).await?;
    if first.success() {
        return Ok(first);
    }
    let suggests_kbi = matches!(
        &first,
        AuthResult::Failure { remaining_methods, .. }
            if remaining_methods.contains(&MethodKind::KeyboardInteractive)
    );
    if suggests_kbi {
        return keyboard_flow(handle, username, password).await;
    }
    Ok(first)
}

async fn keyboard_flow(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    password: &Zeroizing<String>,
) -> Result<AuthResult, SshError> {
    let mut resp = handle
        .authenticate_keyboard_interactive_start(username, None::<String>)
        .await?;
    for _ in 0..MAX_KBI_ROUNDS {
        match resp {
            KeyboardInteractiveAuthResponse::Success => {
                return Ok(AuthResult::Success);
            }
            KeyboardInteractiveAuthResponse::Failure { partial_success, .. } => {
                return Ok(AuthResult::Failure {
                    remaining_methods: MethodSet::empty(),
                    partial_success,
                });
            }
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                // 多于一个提示基本就是 2FA（密码 + 动态码），代填必然错。
                if prompts.len() > 1 {
                    return Err(SshError::InteractiveAuthRequired);
                }
                let answers = prompts.iter().map(|p| {
                    // echo=false 的提示按惯例是口令，填密码；echo=true 的一般是「请输入用户名」
                    if p.echo { username.to_string() } else { password.to_string() }
                }).collect::<Vec<_>>();
                resp = handle.authenticate_keyboard_interactive_respond(answers).await?;
            }
        }
    }
    Err(SshError::InteractiveAuthRequired)
}

async fn publickey_flow(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    key: PrivateKey,
) -> Result<AuthResult, SshError> {
    let hash = rsa_hash(handle, &key).await?;
    let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
    handle.authenticate_publickey(username, with_hash).await.map_err(Into::into)
}

/// RSA 必须协商签名算法；非 RSA 直接 `None`（russh 内部也会忽略）。
async fn rsa_hash(
    handle: &mut Handle<ClientHandler>,
    key: &PrivateKey,
) -> Result<Option<HashAlg>, SshError> {
    if !key.algorithm().is_rsa() {
        return Ok(None);
    }
    // `best_supported_rsa_hash` 返回 `Option<Option<HashAlg>>`：
    // 外层 None = 服务器没宣告 rsa-sha2 支持，内层 None = 只能用 legacy ssh-rsa。
    let best = handle.best_supported_rsa_hash().await?.flatten();
    Ok(best)
}

/// 本平台的 agent 连接类型。russh 把它做成泛型参数（`AgentClient<S>`），
/// 而且 `connect_env` / `connect_named_pipe` 只存在于具体 `S` 的 impl 上，
/// 所以这里必须按平台各起一个别名，不能写成统一类型。
#[cfg(unix)]
type Agent = russh::keys::agent::client::AgentClient<tokio::net::UnixStream>;
#[cfg(windows)]
type Agent = russh::keys::agent::client::AgentClient<
    tokio::net::windows::named_pipe::NamedPipeClient,
>;

/// Windows 上 OpenSSH 的 agent 固定走这条命名管道（服务名 `ssh-agent`）。
/// Pageant（WinSCP 那批用户）需要 russh 的 `pageant` 具体类型，M1 先不接，
/// 用 `Credential::PrivateKey` 显式导入手动的 `.ppk`/PEM 已经能覆盖。
#[cfg(windows)]
const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

async fn agent_flow(
    handle: &mut Handle<ClientHandler>,
    username: &str,
) -> Result<AuthResult, SshError> {
    let mut agent = connect_agent().await?;
    let identities = agent.request_identities().await.map_err(|e| SshError::Russh(e.into()))?;
    if identities.is_empty() {
        // agent 在跑但一把钥匙都没有：这跟「认证失败」要区分，UI 才能提示先 ssh-add
        return Err(SshError::KeyLoad("ssh-agent 里没有任何身份".into()));
    }
    let mut last = AuthResult::Failure {
        remaining_methods: MethodSet::empty(),
        partial_success: false,
    };
    for id in identities {
        // 逐个试：agent 里通常有多把钥匙，服务器只认其中一把是常态
        let pubkey = id.public_key().into_owned();
        let hash = if pubkey.algorithm().is_rsa() {
            handle.best_supported_rsa_hash().await?.flatten()
        } else {
            None
        };
        match handle.authenticate_publickey_with(username, pubkey, hash, &mut agent).await {
            Ok(r) => {
                if r.success() {
                    return Ok(r);
                }
                last = r;
            }
            Err(e) => {
                tracing::debug!(error = %e, "agent 签名失败，换下一把钥匙");
            }
        }
    }
    Ok(last)
}

#[cfg(unix)]
async fn connect_agent() -> Result<Agent, SshError> {
    use russh::keys::agent::client::AgentClient;
    AgentClient::connect_env()
        .await
        .map_err(|e| SshError::KeyLoad(format!("无法连接 ssh-agent（SSH_AUTH_SOCK）: {e}")))
}

#[cfg(windows)]
async fn connect_agent() -> Result<Agent, SshError> {
    use russh::keys::agent::client::AgentClient;
    AgentClient::connect_named_pipe(OPENSSH_AGENT_PIPE)
        .await
        .map_err(|e| SshError::KeyLoad(format!("无法连接 OpenSSH ssh-agent: {e}")))
}

/// 解析私钥。加密 key 没给口令、或口令不对，都归到 `KeyLoad`：
/// 对用户而言这就是「打不开这把钥匙」，而不是「服务器拒绝了你」。
fn load_private_key(
    pem: &Zeroizing<String>,
    passphrase: Option<&Zeroizing<String>>,
    is_ppk: bool,
) -> Result<PrivateKey, SshError> {
    let parsed = if is_ppk {
        PrivateKey::from_ppk(pem.as_str(), passphrase.map(|p| p.to_string()))
            .map_err(|e| SshError::KeyLoad(e.to_string()))?
    } else {
        PrivateKey::from_openssh(pem.as_str())
            .map_err(|e| SshError::KeyLoad(e.to_string()))?
    };
    if !parsed.is_encrypted() {
        return Ok(parsed);
    }
    let pass = passphrase.ok_or_else(|| SshError::KeyLoad("该私钥有口令保护，但未提供口令".into()))?;
    parsed
        .decrypt(pass.as_str())
        .map_err(|_| SshError::KeyLoad("私钥口令不正确".into()))
}

/// 这把 PEM 是不是口令保护的。判定口径与 `load_private_key` 完全一致——
/// 「要不要向用户要口令」必须与「解析时到底需不需要口令」用同一个答案，
/// 否则会出现弹了框却说口令错，或者该弹框却直接报 KeyLoad。
pub fn key_is_encrypted(pem: &str) -> bool {
    match PrivateKey::from_openssh(pem) {
        Ok(parsed) => parsed.is_encrypted(),
        // 连格式都不认，谈不上口令；真正的报错留给认证那一步
        Err(_) => false,
    }
}

/// 供测试与上层复用：把一个 `AuthResult` 摊成人类可读的失败原因。
pub fn describe_failure(auth: &AuthResult) -> &'static str {
    match auth {
        AuthResult::Success => "success",
        AuthResult::Failure { partial_success: true, .. } => "partial",
        AuthResult::Failure { .. } => "rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{
        ENC_ED25519, ENC_ED25519_B64, ENC_ED25519_PASSPHRASE, PLAIN_ED25519, PLAIN_ED25519_B64,
    };
    use dbhx_model::ErrorCode;

    fn zero(s: &str) -> Zeroizing<String> {
        Zeroizing::new(s.to_string())
    }

    #[test]
    fn describe_failure_distinguishes_partial() {
        let a = AuthResult::Failure {
            remaining_methods: MethodSet::empty(),
            partial_success: true,
        };
        assert_eq!(describe_failure(&a), "partial");
    }

    #[test]
    fn unencrypted_key_loads_without_a_passphrase() {
        let k = load_private_key(&zero(PLAIN_ED25519), None, false).unwrap();
        assert_eq!(k.public_key().to_openssh().unwrap(), format!("ssh-ed25519 {PLAIN_ED25519_B64} dbhx-test"));
    }

    #[test]
    fn encrypted_key_without_passphrase_fails_before_any_network_io() {
        let e = load_private_key(&zero(ENC_ED25519), None, false).unwrap_err();
        assert!(matches!(e, SshError::KeyLoad(_)), "{e:?}");
    }

    #[test]
    fn right_passphrase_unlocks_the_same_key_ssh_keygen_made() {
        let k = load_private_key(
            &zero(ENC_ED25519),
            Some(&zero(ENC_ED25519_PASSPHRASE)),
            false,
        )
        .unwrap();
        assert_eq!(k.public_key().to_openssh().unwrap().split(' ').nth(1).unwrap(), ENC_ED25519_B64);
    }

    #[test]
    fn wrong_passphrase_is_key_load_error_not_auth_failure() {
        // 「钥匙口令错」和「服务器拒绝你」是两件事：前者本地就能修，后者要问运维。
        let e = load_private_key(&zero(ENC_ED25519), Some(&zero("wrong")), false).unwrap_err();
        assert!(matches!(e, SshError::KeyLoad(_)), "{e:?}");
        assert!(e.to_string().contains("私钥口令不正确"), "{e}");
    }

    #[test]
    fn garbled_pem_is_key_load_error() {
        assert!(matches!(load_private_key(&zero("not a key"), None, false), Err(SshError::KeyLoad(_))));
    }

    #[test]
    fn key_load_surfaces_as_permission_denied_over_ipc() {
        let ipc: dbhx_model::IpcError = SshError::KeyLoad("打不开钥匙".into()).into();
        assert_eq!(ipc.code, ErrorCode::PermissionDenied);
    }
}
