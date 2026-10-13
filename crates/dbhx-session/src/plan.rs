//! 拨号计划：把「用什么凭据、怎么裁主机密钥」打包成一次性值对象。
//!
//! 会话层拿到的永远是这个结构，而不是 vault 的引用——它不需要知道密文怎么解密、
//! 口令存在哪一行。反过来说，凭据来源（`PlanSource`）也**不需要知道池的存在**，
//! 它只对着身份键工作。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dbhx_ssh::{Credential, HostKeyVerifier};

use crate::error::SessionError;
use crate::key::TransportKey;

/// 一次拨号所需的全部材料。
///
/// 刻意不 `derive(Debug)`：`credential` 是私钥文本或口令，一次意外的 `{:?}` 就进了日志。
/// 需要留痕时打印 `TransportKey::describe()`，那里没有秘密。
pub struct DialPlan {
    pub credential: Credential,
    /// 主机密钥的裁决者。首次连接时它该问 UI，后台免登录复用（Strict 策略，无人可问）
    /// 时它该直接拒绝未知密钥——「谁来裁」由凭据来源决定，不由池决定。
    pub verifier: Box<dyn HostKeyVerifier>,
    /// `None` 表示不发 keepalive。它同时是身份键的一部分，两处必须来自同一个源，
    /// 否则键判等与实际配置会分叉。
    pub keepalive: Option<Duration>,
}

/// 凭据来源。每次拨号调用一次，因此实现必须可重复调用（`Fn` 而非 `FnOnce`）。
///
/// 返回 boxed future 而不是 async fn：不引 `async-trait` 依赖，同时把「等用户点确认」
/// 这种无上限的等待写进类型。实现方要先 `clone` 出需要的 id——身份键在这里是按值给的，
/// 就是为了允许 future 捕获它。
pub type PlanSource = Box<
    dyn Fn(TransportKey) -> Pin<Box<dyn Future<Output = Result<DialPlan, SessionError>> + Send>>
        + Send
        + Sync,
>;
