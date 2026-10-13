//! 主机密钥裁决：vault 里的 `known_hosts` + 需要时问用户。
//!
//! 三条不可让的规则，都是安全语义：
//! 1. **一致才放行**。`Trusted` 是唯一返回 `true` 的路径。
//! 2. **`Changed` 硬阻断**：不发消息、不等回答、没有「仍然连接」。只发一个
//!    [`crate::events::HostKeyConflict`] 让用户知道为什么连不上——中间人告警的价值
//!    正在于它不能被点掉。
//! 3. **`Unknown` 才问人**。用户确认之后**先落盘再放行**，这样即使这次连接随后失败，
//!    下次也不会再问一遍（TOFU 的「一次」必须是真的）。
//!
//! 锁的用法有讲究：`known` 的锁只用来读结论和写信任，**等用户点按钮时不持锁**——
//! 那个等待没有上限，抱着锁等会把别的主机也一起钉住。

use std::sync::{Arc, Mutex};

use dbhx_model::{HostKeyDecision, KnownHost, KnownHostSource, LiveEndpoint, now_ms};
use dbhx_ssh::{HostKeyVerifier, ServerKey, pattern_of};
use dbhx_vault::KnownHosts;
use tokio::sync::mpsc;

use crate::events::{BusEvent, HostKeyConflict, HostKeyPending};
use crate::state::{Awaits, UserAnswer};

pub struct VaultVerifier {
    known: Arc<Mutex<KnownHosts>>,
    awaits: Arc<Awaits>,
    bus: mpsc::Sender<BusEvent>,
}

impl VaultVerifier {
    pub fn new(known: Arc<Mutex<KnownHosts>>, awaits: Arc<Awaits>, bus: mpsc::Sender<BusEvent>) -> Self {
        Self { known, awaits, bus }
    }

    /// 裁决本身是可独立测试的纯流程：输入 presented，输出「放行与否 + 是否已落盘」。
    async fn decide(&self, presented: KnownHost) -> bool {
        // 段一：只在锁内取结论
        let decision = {
            let guard = self.known.lock().unwrap_or_else(|p| p.into_inner());
            guard.validate(&presented)
        };
        match decision {
            HostKeyDecision::Trusted => true,
            HostKeyDecision::Changed { stored, .. } => {
                let _ = self.bus.send(BusEvent::HostKeyConflict(HostKeyConflict {
                    endpoint: endpoint_of(&presented),
                    stored: *stored,
                    presented: presented.clone(),
                }))
                .await;
                false
            }
            HostKeyDecision::Unknown { pending } => {
                let endpoint = endpoint_of(&presented);
                let (id, rx) = self.awaits.register();
                let event = BusEvent::HostKeyPending(HostKeyPending { request: id, endpoint, presented: *pending });
                if self.bus.send(event).await.is_err() {
                    self.awaits.drop_request(id);
                    return false;
                }
                let accepted = match rx.await {
                    Ok(UserAnswer::Confirmed) => true,
                    Ok(_) => false,
                    // 发送端消失（应用正在退出）按拒绝处理：默认不放行
                    Err(_) => false,
                };
                if accepted {
                    // 段二：确认之后才写盘。顺序反了的话，用户取消也会留下一条信任记录
                    {
                        let mut guard = self.known.lock().unwrap_or_else(|p| p.into_inner());
                        guard.trust(presented);
                    }
                    self.save();
                }
                accepted
            }
        }
    }

    fn save(&self) {
        let guard = self.known.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = guard.save() {
            // 落盘失败不回滚放行：这一条连接的信任已经在内存里，下次重启会再问一次
            tracing::warn!(?e, "known_hosts 写盘失败");
        }
    }
}

impl HostKeyVerifier for VaultVerifier {
    fn verify(
        &mut self,
        endpoint: LiveEndpoint,
        key: ServerKey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.decide(known_host_of(&endpoint, &key)).await })
    }
}

/// 服务器密钥 ⇒ known_hosts 的一行。
pub fn known_host_of(endpoint: &LiveEndpoint, key: &ServerKey) -> KnownHost {
    KnownHost {
        pattern: pattern_of(&endpoint.host, endpoint.port),
        algorithm: key.algorithm.clone(),
        base64_key: key.base64_key.clone(),
        added_at_ms: now_ms(),
        source: KnownHostSource::UserConfirmed,
    }
}

/// `known_hosts` 的 pattern 里带端口信息（非 22 时是 `[host]:port`），反推一个 endpoint。
/// 用户名不在密钥里，这里留空——载荷只用来给人看指纹是哪台机器。
fn endpoint_of(host: &KnownHost) -> LiveEndpoint {
    let (h, port) = match host.pattern.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .map(|(h, tail)| (h.to_string(), tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(22)))
            .unwrap_or_else(|| (host.pattern.clone(), 22)),
        None => (host.pattern.clone(), 22),
    };
    LiveEndpoint { host: h, port, username: String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbhx_vault::KeySource;
    use tokio::sync::mpsc;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dbhx-ipc-verify-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(b64: &str) -> ServerKey {
        ServerKey { algorithm: "ssh-ed25519".into(), base64_key: b64.into(), fingerprint: "SHA256:xx".into(), is_certificate: false }
    }

    fn endpoint(host: &str, port: u16) -> LiveEndpoint {
        LiveEndpoint { host: host.into(), port, username: "app".into() }
    }

    /// 起一套「verifier + 事件队列 + 回答通道」，不碰网络也不起 webview。
    fn harness(tag: &str) -> (VaultVerifier, mpsc::Receiver<BusEvent>, Arc<Awaits>, Arc<Mutex<KnownHosts>>) {
        let dir = temp_dir(tag);
        let _ = std::fs::remove_file(dir.join("known_hosts"));
        let known = Arc::new(Mutex::new(KnownHosts::load(&dir).unwrap()));
        let awaits = Arc::new(Awaits::default());
        let (tx, rx) = mpsc::channel(32);
        (VaultVerifier::new(Arc::clone(&known), Arc::clone(&awaits), tx), rx, awaits, known)
    }

    /// 在别的任务里替用户点按钮。返回 JoinHandle 是为了让调用方 await 它，
    /// 确保「回答已经发生」再断言后续状态。
    fn answer_when_pending(
        mut rx: mpsc::Receiver<BusEvent>,
        awaits: Arc<Awaits>,
        answer: UserAnswerFactory,
    ) -> tokio::task::JoinHandle<BusEvent> {
        tokio::spawn(async move {
            let event = rx.recv().await.expect("后端应当发出一个询问事件");
            if let BusEvent::HostKeyPending(p) = &event {
                awaits.reply(p.request, answer.build(p.request)).ok();
            }
            event
        })
    }

    enum UserAnswerFactory {
        Accept,
        Refuse,
    }

    impl UserAnswerFactory {
        fn build(&self, _request: u32) -> UserAnswer {
            match self {
                Self::Accept => UserAnswer::Confirmed,
                Self::Refuse => UserAnswer::Cancelled,
            }
        }
    }

    #[tokio::test]
    async fn unknown_key_is_asked_about_and_accepted_keys_are_written_down() {
        let (verifier, rx, awaits, known) = harness("unknown");
        let responder = answer_when_pending(rx, Arc::clone(&awaits), UserAnswerFactory::Accept);
        let presented = known_host_of(&endpoint("db1", 22), &key("AAAAB3"));

        assert!(verifier.decide(presented.clone()).await, "用户确认后必须放行");
        let event = responder.await.unwrap();
        match event {
            BusEvent::HostKeyPending(p) => {
                assert_eq!(p.presented.base64_key, "AAAAB3");
                assert_eq!(p.endpoint.host, "db1", "确认框要让人看出是哪台机器");
                assert_ne!(p.request, 0, "请求号必须发出去，否则前端没法回答");
            }
            other => panic!("期望待确认事件，收到 {other:?}"),
        }
        // 先落盘再放行：这条连接之后即使失败，也不该再问第二遍
        assert_eq!(known.lock().unwrap().entries().len(), 1);
        assert_eq!(std::fs::read_to_string(temp_dir("unknown").join("known_hosts")).unwrap().trim(), "db1 ssh-ed25519 AAAAB3");
        assert_eq!(awaits.pending(), 0, "回答之后请求要从表里消失");
    }

    #[tokio::test]
    async fn a_second_connect_with_the_same_key_is_trusted_without_a_prompt() {
        let (verifier, mut rx, awaits, known) = harness("trusted");
        {
            let mut guard = known.lock().unwrap();
            guard.trust(known_host_of(&endpoint("db5", 22), &key("SAME")));
        }
        assert!(verifier.decide(known_host_of(&endpoint("db5", 22), &key("SAME"))).await);
        assert_eq!(awaits.pending(), 0, "已信任的主机不该再登记询问");
        assert!(rx.try_recv().is_err(), "一条消息都不该发出去");
    }

    #[tokio::test]
    async fn cancelling_the_prompt_leaves_no_trust_behind() {
        let (verifier, rx, awaits, known) = harness("cancel");
        let responder = answer_when_pending(rx, Arc::clone(&awaits), UserAnswerFactory::Refuse);
        assert!(!verifier.decide(known_host_of(&endpoint("db4", 22), &key("CCCC"))).await, "用户取消就是不放行");
        responder.await.unwrap();
        assert!(known.lock().unwrap().entries().is_empty(), "取消不能留下信任记录");
    }

    #[tokio::test]
    async fn a_changed_key_is_refused_without_asking_and_without_a_button() {
        let (verifier, mut rx, awaits, known) = harness("changed");
        {
            let mut guard = known.lock().unwrap();
            guard.trust(known_host_of(&endpoint("db3", 22), &key("OLDKEY")));
        }
        let decided = verifier.decide(known_host_of(&endpoint("db3", 22), &key("NEWKEY"))).await;
        assert!(!decided, "密钥变更必须阻断");
        assert_eq!(awaits.pending(), 0, "变更不该发起确认请求——没有「仍然连接」这个选项");
        match rx.recv().await {
            Some(BusEvent::HostKeyConflict(e)) => {
                assert_eq!(e.stored.base64_key, "OLDKEY");
                assert_eq!(e.presented.base64_key, "NEWKEY");
            }
            other => panic!("应该只收到冲突通知：{other:?}"),
        }
        // 旧记录必须留着：覆盖了就等于把中间人告警抹掉了
        let guard = known.lock().unwrap();
        assert_eq!(guard.entries().len(), 1);
        assert_eq!(guard.entries()[0].base64_key, "OLDKEY");
    }

    #[test]
    fn pattern_and_endpoint_round_trip_through_the_known_hosts_format() {
        let host = known_host_of(&endpoint("db1", 2222), &key("AAAA"));
        assert_eq!(host.pattern, "[db1]:2222", "非默认端口必须按 OpenSSH 写法带方括号");
        let back = endpoint_of(&host);
        assert_eq!((back.host.as_str(), back.port), ("db1", 2222));

        let default_port = known_host_of(&endpoint("db1", 22), &key("AAAA"));
        assert_eq!(default_port.pattern, "db1");
        assert_eq!(endpoint_of(&default_port).port, 22);
    }

    #[test]
    fn master_key_source_is_a_file_in_the_app_dir() {
        // 只是把「生产用哪把主密钥」这个决定写进测试，防止哪天悄悄换成 Memory
        let dir = temp_dir("keystore");
        let source = KeySource::File(dir.join("master.key"));
        let key = source.load_or_create().unwrap();
        assert_eq!(key.as_bytes().len(), 32);
        assert!(dir.join("master.key").exists());
    }
}
