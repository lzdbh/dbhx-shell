//! 唯一的网络出口。
//!
//! 把连接、超时、TLS、令牌放置位置这些容易出事的事集中到一处，
//! 好处是「令牌不会出现在 URL 里」这种话可以当场验证，而不是靠每个 adapter 自觉。
//! 错误类型只到「能不能重试、该跟用户怎么说」这个粒度：底层错误对象的 Display
//! 会把完整 URL（含 query）带出来，一旦转成文案就可能把令牌送进日志，所以一律不往下传。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dbhx_model::{redacted, ErrorCode, IpcError};
use zeroize::Zeroizing;

/// 一次请求的整体上限。同步是一次用户主动点击的操作，不是后台任务，
/// 所以宁可让它明确失败并告诉用户可以重试，也不要让它吊在那里转圈。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 装箱的 future。用它而不是 `async_trait`：`Transport` 要保持 object-safe，
/// 好让 `&dyn Transport` 直接当参数传——标准库自己就能写这个签名，不必再引一个宏。
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// 一次要发出去的请求。
///
/// `token` 单独一个字段而不是「调用方自己拼好 header」：这样「令牌进 URL」
/// 这条路只有 `send` 一个实现说了算，测试里也只需要盯一处。
pub struct Request {
    pub method: Method,
    pub url: String,
    pub token: Zeroizing<String>,
    /// JSON 请求体。`None` = 无体（GET）。
    pub body: Option<serde_json::Value>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 调试打印会出现在 panic 信息和偶发的日志里：令牌与完整 URL 都不能进去
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("host", &url_host(&self.url))
            .field("token", &redacted("token"))
            .field("has_body", &self.body.is_some())
            .finish()
    }
}

/// 只取 URL 的主机名，供日志和 `Debug` 用。
///
/// 刻意不打印 path：片段 id 虽然不是秘密，但 query 在某些接口里会带凭据，
/// 而日志脱敏只能遮今天已知的形状。
fn url_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme.split(['/', '?', '#']).next().unwrap_or("").to_string()
}

/// 读回来的响应。`json` 已经解析过，非 JSON 的响应直接算 `BadBody`；
/// 只有 2xx 的空响应体（`DELETE` 的 204）会是 `Null`。
pub struct Response {
    pub status: u16,
    pub json: serde_json::Value,
}

/// 云同步能遇到的传输层失败。
///
/// 每一档都对应一句能直接给用户看的话：把「403」原样抛出去，用户只能去猜是
/// 令牌错了还是被限流了，而这两件事的下一步操作完全不同。
#[derive(Debug, Clone, thiserror::Error)]
pub enum WireError {
    /// 连不上、DNS 失败、证书被拒。`reason` 是粗分类，不含 URL。
    #[error("传输失败：{reason}")]
    Connect { reason: String, retryable: bool },
    #[error("等待云端响应超时（{secs} 秒）")]
    TimedOut { secs: u64 },
    /// 两家都要求令牌能读写「片段」这一类资源。具体在各自的授权页面上叫什么名字，
    /// 没实测过（测试用的令牌是整账号权限的私人令牌），所以文案只说要做的事，
    /// 不编一个可能对不上的勾选项名字。
    #[error("令牌无效，或者它没有片段（gist）的读写权限")]
    Unauthorized,
    #[error("被云端限流，请稍后再试")]
    RateLimited,
    /// 云端说不存在。对「拉取」而言这不是失败（第一次同步时本来就没有文件），
    /// 所以它只在「带着 id 却查不到」这种确凿的场合出现。
    #[error("云端没有这个片段（可能已在网页上被删掉）")]
    NotFound,
    #[error("响应读不懂：接口改了，或被中间人改写了")]
    BadBody,
    #[error("云端返回 HTTP {status}")]
    Status { status: u16, retryable: bool },
}

impl WireError {
    /// 这一类错误值不值得让用户再点一次「同步」。
    pub fn retryable(&self) -> bool {
        match self {
            WireError::Connect { retryable, .. } => *retryable,
            WireError::TimedOut { .. } | WireError::RateLimited | WireError::Status { .. } => true,
            WireError::Unauthorized
            | WireError::NotFound
            | WireError::BadBody => false,
        }
    }

    /// 前端能据此分支的稳定错误码（见 `frontend/src/lib/ipc/errors.ts`）。
    fn code(&self) -> ErrorCode {
        match self {
            WireError::Connect { .. } => ErrorCode::Unreachable,
            WireError::TimedOut { .. } => ErrorCode::Timeout,
            // 令牌被云端拒掉就是认证失败，不能算 `PermissionDenied`：后者在这条路径上
            // 已经是「本机解不开云上那份密文」（`VaultError::Crypto`）的专用码，
            // 两件事挤进同一个 code，前端就没法把「去改令牌」和「去改同步串」分开说。
            WireError::Unauthorized => ErrorCode::AuthFailed,
            // 限流和「超预算」在用户那一侧是同一件事：等一会儿再来
            WireError::RateLimited => ErrorCode::QuotaExceeded,
            WireError::NotFound => ErrorCode::NotFound,
            WireError::BadBody | WireError::Status { .. } => ErrorCode::Internal,
        }
    }
}

impl From<WireError> for IpcError {
    fn from(e: WireError) -> Self {
        IpcError::new(e.code(), e.to_string(), e.retryable())
    }
}

/// 传输层。真实实现是 `HttpTransport`，测试用进程内的假实现：
/// 有了这层接缝，两家的 URL/载荷差异、令牌放置位置、各种状态码的归类
/// 全部能离线测完，不需要真建片段。
pub trait Transport: Send + Sync {
    fn send(&self, req: Request) -> BoxFuture<Result<Response, WireError>>;
}

/// 走真实网络的实现。
pub struct HttpTransport {
    client: reqwest::Client,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    /// TLS 一律用**系统信任库**（见 Cargo.toml 里 reqwest 那段）：
    /// 装了 Steam++ 或公司 SSL 审查网关的机器上，证书链由那个网关签发，
    /// 根证书只进了系统库。用 rustls 自带的 webpki-roots 会让这些用户同步全挂，
    /// 而且报错只是「证书错误」。
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("dbhx-shell/", env!("CARGO_PKG_VERSION")))
            // 跳转规则要自己管：令牌放在 header 里，而 header 会跟着请求送到跳转的目标。
            // 一次把请求从 https 降到 http 的跳转就等于把令牌明文发出去，所以只跟 https。
            // `stop()` 的语义是「把 30x 原样返回」，下面 `send_once` 会因此报 BadBody——
            // 宁可失败得难看，也不能悄悄把凭据交给下一个地址。
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() == "https" {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            // builder 只在配置非法时失败，这里几项都是常量
            .expect("reqwest client 配置非法");
        Self { client }
    }
}

impl Transport for HttpTransport {
    fn send(&self, req: Request) -> BoxFuture<Result<Response, WireError>> {
        // `BoxFuture` 是 `'static` 的（这样才能把 `&dyn Transport` 当参数传而不必给
        // trait 方法加生命周期），所以不能在这里借用 `&self.client`。
        // reqwest 的 Client 内部是一个 Arc，clone 只加一次引用计数。
        let client = self.client.clone();
        Box::pin(async move { send_once(&client, req).await })
    }
}

/// 从 trait 方法里拆出来，是为了让 `Request` 的所有权移动写在一个普通 async fn 里，
/// 而不是塞进 `Box::pin(async move { ... })` 的大块头里。
async fn send_once(client: &reqwest::Client, req: Request) -> Result<Response, WireError> {
    let Request { method, url, token, body } = req;
    // 当场验证「令牌不进 URL」。今天两家都不需要 query 鉴权，
    // 而一旦有人为了省事把令牌拼进 URL，日志脱敏只能遮住日志，遮不住对端。
    if url.contains(token.as_str()) {
        return Err(WireError::Connect {
            reason: "内部检查：令牌出现在请求地址里".into(),
            retryable: false,
        });
    }
    let method_name = method.as_str();
    let call_method = match method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Patch => reqwest::Method::PATCH,
        Method::Delete => reqwest::Method::DELETE,
    };
    let mut call = client
        .request(call_method, &url)
        .header("Authorization", format!("Bearer {}", token.as_str()))
        // 两家都只认这个：不声明的话，某些中间层会把错误页当 HTML 返回来
        .header("Accept", "application/json");
    if let Some(body) = body {
        call = call.json(&body);
    }
    let sent = tokio::time::timeout(REQUEST_TIMEOUT, call.send()).await;
    let resp = match sent {
        Err(_) => return Err(WireError::TimedOut { secs: REQUEST_TIMEOUT.as_secs() }),
        Ok(Err(e)) => return Err(classify(&e)),
        Ok(Ok(r)) => r,
    };
    let status = resp.status().as_u16();
    let text = match tokio::time::timeout(REQUEST_TIMEOUT, resp.text()).await {
        Err(_) => return Err(WireError::TimedOut { secs: REQUEST_TIMEOUT.as_secs() }),
        Ok(Err(e)) => return Err(classify(&e)),
        Ok(Ok(t)) => t,
    };
    tracing::info!(
        method = %method_name,
        host = %url_host(&url),
        status,
        "云同步请求"
    );
    // 先按空/非空分开：`DELETE` 的 204 本来就没有体，那是成功而不是读不懂
    let empty = text.trim().is_empty();
    let json: serde_json::Value = if empty {
        serde_json::Value::Null
    } else {
        match serde_json::from_str(text.trim()) {
            Ok(v) => v,
            Err(_) => return Err(WireError::BadBody),
        }
    };
    if status == 401 || status == 403 {
        // GitHub 的限流也是 403，靠响应里的 message 区分；两家都带这个字段
        let msg = json.get("message").and_then(|m| m.as_str()).unwrap_or("");
        if msg.to_ascii_lowercase().contains("rate limit") {
            return Err(WireError::RateLimited);
        }
        return Err(WireError::Unauthorized);
    }
    if status == 404 {
        return Err(WireError::NotFound);
    }
    if !(200..300).contains(&status) {
        return Err(WireError::Status {
            status,
            retryable: status >= 500,
        });
    }
    // 2xx 且没有体（`DELETE` 的 204 就是这样）也走到这里：`json` 是 `Null`，
    // 需要读回内容的调用会因为字段缺失报 `UnexpectedShape`，
    // 而这一层不为某一家接口特判「哪种空响应算正常」。
    Ok(Response { status, json })
}

/// 把 reqwest 的错误归到粗分类。刻意不用 `e.to_string()`：
/// 它的 Display 里带完整 URL，而错误文案会一路走到日志和 IPC 返回值。
fn classify(e: &reqwest::Error) -> WireError {
    if e.is_timeout() {
        return WireError::TimedOut { secs: REQUEST_TIMEOUT.as_secs() };
    }
    if e.is_builder() {
        return WireError::Connect { reason: "请求地址不合法".into(), retryable: false };
    }
    if e.is_connect() {
        // 证书被拒是这一类里最常见的一种：本机装了 SSL 拦截类软件时必然走到这里
        return WireError::Connect {
            reason: "连不上云端（网络不通，或本机证书链被拦截软件改写）".into(),
            retryable: true,
        };
    }
    if e.is_decode() {
        return WireError::BadBody;
    }
    if e.is_body() || e.is_request() {
        return WireError::Connect { reason: "请求没能发出去".into(), retryable: true };
    }
    WireError::Connect { reason: "传输失败".into(), retryable: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_categories_map_to_the_codes_the_frontend_can_act_on() {
        let cases = [
            (WireError::Unauthorized, ErrorCode::AuthFailed),
            (WireError::RateLimited, ErrorCode::QuotaExceeded),
            (WireError::NotFound, ErrorCode::NotFound),
            (WireError::BadBody, ErrorCode::Internal),
            (
                WireError::Connect { reason: "x".into(), retryable: true },
                ErrorCode::Unreachable,
            ),
            (WireError::TimedOut { secs: 30 }, ErrorCode::Timeout),
        ];
        for (err, code) in cases {
            let ipc: IpcError = err.into();
            assert_eq!(ipc.code, code);
        }
    }

    /// 「令牌被云端拒了」和「本机解不开云上那一份」必须落在不同 code 上。
    /// 这两个码在同步这条路径上是前端唯一的分支依据——撞上之后只能猜，
    /// 猜错就是把去换令牌的人支去重贴同步串。
    #[test]
    fn a_rejected_token_is_not_the_same_failure_as_an_undecryptable_cloud_copy() {
        let token: IpcError = WireError::Unauthorized.into();
        let sealed: IpcError = dbhx_vault::VaultError::Crypto.into();
        assert_ne!(token.code, sealed.code, "{:?} 与 {:?} 撞了", token.code, sealed.code);
    }

    /// 错误文案是给用户看的，也是唯一可能被写进日志的东西：一次都不能带出令牌。
    #[test]
    fn no_error_message_can_carry_a_token_or_a_url() {
        let errs = [
            WireError::Connect { reason: "连不上 api.github.com".into(), retryable: true },
            WireError::TimedOut { secs: 30 },
            WireError::Unauthorized,
            WireError::RateLimited,
            WireError::NotFound,
            WireError::BadBody,
            WireError::Status { status: 502, retryable: true },
        ];
        for e in errs {
            let text = e.to_string();
            assert!(!text.contains("Bearer"), "{text}");
            assert!(!text.contains("access_token"), "{text}");
            assert!(!text.contains("https://"), "{text}");
        }
    }

    /// `Request` 是整条链路上唯一同时持有令牌和地址的对象，
    /// 而 `{:?}` 是它最容易被顺手写进日志或 panic 信息的地方。
    #[test]
    fn request_debug_shows_neither_token_nor_full_url() {
        let token = "ghp_Sup3rS3cr3tTok3nValue000000";
        let req = Request {
            method: Method::Post,
            url: format!("https://api.github.com/gists?access_token={token}"),
            token: Zeroizing::new(token.to_string()),
            body: Some(serde_json::json!({ "files": { "x": { "content": "密文" } } })),
        };
        let rendered = format!("{req:?}");
        assert!(!rendered.contains(token), "Debug 泄露了令牌: {rendered}");
        assert!(!rendered.contains("api.github.com/gists?"), "Debug 泄露了完整地址: {rendered}");
        assert!(rendered.contains("api.github.com"), "Debug 该留下主机名才好排障: {rendered}");
    }

    #[test]
    fn host_extraction_stops_at_path_query_and_missing_scheme() {
        assert_eq!(url_host("https://api.github.com/gists/abc"), "api.github.com");
        assert_eq!(url_host("https://api.github.com/gists?x=1"), "api.github.com");
        assert_eq!(url_host("https://api.github.com#frag"), "api.github.com");
        assert_eq!(url_host("api.github.com/gists"), "api.github.com");
    }

    #[test]
    fn only_the_recoverable_kinds_are_worth_a_retry() {
        assert!(WireError::TimedOut { secs: 30 }.retryable());
        assert!(WireError::RateLimited.retryable());
        assert!(!WireError::Unauthorized.retryable(), "令牌错重试一百次还是错");
        assert!(!WireError::BadBody.retryable());
    }
}
