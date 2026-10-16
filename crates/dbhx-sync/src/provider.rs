//! 云端那一份的形状：地址、请求体、响应解析。
//!
//! 这一层只搬字节：交进去的是已经封好的信封，读回来的也是同样封着的信封。
//! 派生、加解密、合并一概不在这里 —— 于是两家的字段差异能毫秒级离线测完，
//! 而真联网的用例整个 crate 只留一条 `#[ignore]` 的（见 `tests` 末尾）。
//!
//! 为什么文件内容还要再套一层 base64：信封是任意字节，而片段接口的 `content`
//! 是 JSON 字符串。非 UTF-8 的密文直接塞进去，会在「把请求体序列化出来」这一步
//! 就变成一串替换字符 —— 那种损坏要等到用户换设备拉取时才暴露，代价太大。
//! 体积因此涨 4/3，这个代价换的是「任何接口都能原样往返」，值得。
//!
//! 关于「两家到底差在哪」：实测下来只差**域名**。GitHub 与 Gitee 的 `files` 都是
//! 「文件名 → 内容」的哈希，创建与更新都是这个形状，读回来都带 `content` 与
//! `truncated`。这不是照文档推的：Gitee 的更新接口对平铺的 `filename`+`content`
//! 回 200 却一个字也不改（`tools/sync-probe.mjs` 里那条探测），而按 GitHub 那套
//! `files` 哈希才真的写进去了。共用一条路径的前提是这样量出来的。

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use dbhx_model::{redacted, ErrorCode, IpcError, ProviderKind, SyncTarget};
use zeroize::Zeroizing;

use crate::wire::{Method, Request, Transport, WireError};

/// 云端那个片段文件的固定文件名。两家接口都按文件名取内容，改名等于另存了一份数据，
/// 所以它一旦定下来就不能再动 —— 老设备上的片段全靠这个名字认出来。
pub const SYNC_FILE_NAME: &str = "dbhx-shell-sync.json";

/// 单个片段文件的体积上限，按**编码成 base64 之前**的信封字节数算。
///
/// 实测 GitHub 的接口对单文件返回的上限是 1 MiB，而 base64 把 3 字节撑成 4：
/// 700 KiB 编码后约 955,000 字符，留了余量。超它的量宁可在本地就拒绝并说明原因，
/// 也不要发出去换回一个「响应读不懂」—— 用户看到后者只会怀疑同步串错了。
///
/// Gitee 也量过：2 MiB 的文件写得进去、读得回来，`truncated` 还是 false，
/// 所以它不是紧的那一家。两家共用这一个数 —— 信封是同一套，限额没必要分。
const MAX_BLOB_BYTES: usize = 700 * 1024;

/// 片段标题。刻意写死成一句不含身份信息的话：GitHub 的 private gist
/// **标题与描述同样匿名可读**（实测），带上机器名或账号就等于另外泄露一份清单。
const GIST_DESCRIPTION: &str = "dbhx-shell 加密主机列表：内容已密封，需要本机的同步串才能读";

const GITHUB_API: &str = "https://api.github.com";

/// Gitee 的开放接口前缀。实测 `Authorization: Bearer <令牌>` 与 `token <令牌>` 都认，
/// 所以沿用传输层那一种放法，不必把令牌拼进 query —— 一拼进 URL 就再也擦不干净了。
const GITEE_API: &str = "https://gitee.com/api/v5";

/// 从云端读回来的那一份密文。`bytes` 是完整的信封（头部 + AES-GCM），
/// 交给 `dbhx_vault::DerivedKey::from_blob` 才能变成快照。
#[derive(Debug, Clone)]
pub struct CloudBlob {
    pub bytes: Vec<u8>,
    /// 云端报的最后修改时间，原样留着（两家格式不同，只用于日志与「这份是不是我刚写的」）。
    pub updated_at: Option<String>,
}

/// 云端读写能遇到的失败。
///
/// 与 `WireError` 的分工：那边管「这个请求有没有成功」，这边管
/// 「成功回来的东西是不是我们要的那份数据」。分成两个类型是因为处置完全不同 ——
/// 404 对第一次同步来说是正常的「云上还没有」，对接了 id 之后就是「被人删了」。
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("本地记着的片段在云端已经不存在了，需要重新创建一份")]
    Missing,
    #[error("片段地址不合法：只允许字母、数字和 `-` `.` `_`，且不超过 128 个字符")]
    BadId,
    #[error("这份数据有 {used} 字节，超过云端单文件约 {cap} 字节的上限；先删掉些主机或私钥再同步")]
    TooLarge { used: usize, cap: usize },
    #[error("云端把这份内容截断了，拿不到完整数据（文件太大，接口不返回全文）")]
    Truncated,
    #[error("云端响应里没有预期的字段，接口可能改了")]
    UnexpectedShape,
}

impl From<ProviderError> for IpcError {
    fn from(e: ProviderError) -> Self {
        match &e {
            ProviderError::Wire(w) => w.clone().into(),
            ProviderError::Missing => IpcError::new(ErrorCode::NotFound, e.to_string(), false),
            ProviderError::BadId => IpcError::new(ErrorCode::InvalidInput, e.to_string(), false),
            ProviderError::TooLarge { .. } | ProviderError::Truncated => {
                IpcError::new(ErrorCode::QuotaExceeded, e.to_string(), false)
            }
            ProviderError::UnexpectedShape => IpcError::new(ErrorCode::Internal, e.to_string(), false),
        }
    }
}

/// 一家片段服务的客户端。
///
/// `id` 是 `Option`：一台新设备上用户只抄来了同步串，云上还没有我们能认出的片段，
/// 这时 `push` 走创建、`fetch` 直接返回「云上还没有」而不碰网络。
#[derive(Clone)]
pub struct SyncClient {
    provider: ProviderKind,
    id: Option<String>,
    token: Zeroizing<String>,
}

impl std::fmt::Debug for SyncClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncClient")
            .field("provider", &self.provider)
            .field("id", &self.id)
            .field("token", &redacted("token"))
            .finish()
    }
}

impl SyncClient {
    pub fn new(provider: ProviderKind, id: Option<String>, token: Zeroizing<String>) -> Self {
        Self { provider, id: id.filter(|i| !i.trim().is_empty()), token }
    }

    /// 从已存的目标建客户端。令牌按参数进来、不留存：它的生命周期该由调用层
    /// （每次从系统密钥库或用户输入现取）决定，而不是被一个长期存在的结构体拖着。
    pub fn from_target(target: &SyncTarget, token: Zeroizing<String>) -> Self {
        Self::new(target.provider, Some(target.id.clone()), token)
    }

    pub fn provider(&self) -> ProviderKind {
        self.provider
    }

    /// 当前指向的目标。`None` = 还没建过片段，第一次 `push` 成功后才有值。
    pub fn target(&self) -> Option<SyncTarget> {
        self.id.clone().map(|id| SyncTarget { provider: self.provider, id })
    }

    /// 读云端那一份。`Ok(None)` 表示「云上还没有我们能认的东西」，不是失败：
    /// 第一次同步、以及用户在网页上把那个文件删了，都落在这一档。
    pub async fn fetch(&self, transport: &dyn Transport) -> Result<Option<CloudBlob>, ProviderError> {
        let id = match &self.id {
            None => return Ok(None),
            Some(id) => id.clone(),
        };
        let req = self.request(Method::Get, Some(&id), None)?;
        let resp = transport.send(req).await.map_err(gone_as_missing)?;
        parse_cloud(self.provider, &resp.json)
    }

    /// 把一份信封写上云端。没有 id 就创建，有就原地更新 —— 调用方不需要知道这个分岔，
    /// 只要把返回的新 `SyncTarget` 存下来（创建那条路径的 id 是刚拿到的）。
    pub async fn push(&self, transport: &dyn Transport, blob: &[u8]) -> Result<SyncTarget, ProviderError> {
        if blob.len() > MAX_BLOB_BYTES {
            return Err(ProviderError::TooLarge { used: blob.len(), cap: MAX_BLOB_BYTES });
        }
        let content = encode(blob);
        let req = match &self.id {
            None => self.request(Method::Post, None, Some(create_body(self.provider, &content)))?,
            Some(id) => self.request(Method::Patch, Some(id), Some(update_body(self.provider, &content)))?,
        };
        let resp = transport.send(req).await.map_err(gone_as_missing)?;
        let id = parse_id(self.provider, &resp.json)?;
        Ok(SyncTarget { provider: self.provider, id })
    }

    /// 删掉云端那一份。`sync_off` 里用：用户关掉同步后，云上不该继续留着一份密文。
    /// 已经不存在了就算删成功——用户要的结果达成了，不必为一个 404 报错。
    pub async fn delete(&self, transport: &dyn Transport) -> Result<(), ProviderError> {
        let Some(id) = self.id.clone() else { return Ok(()) };
        let req = self.request(Method::Delete, Some(&id), None)?;
        match transport.send(req).await {
            Ok(_) => Ok(()),
            Err(WireError::NotFound) => Ok(()),
            Err(e) => Err(ProviderError::Wire(e)),
        }
    }

    fn request(&self, method: Method, id: Option<&str>, body: Option<serde_json::Value>) -> Result<Request, ProviderError> {
        Ok(Request {
            method,
            url: self.url_for(id)?,
            token: self.token.clone(),
            body,
        })
    }

    /// 地址只能由「固定的服务名 + 白名单内的 id」拼出来：令牌是放在 header 里的，
    /// 而 header 会跟着请求送到地址上的那台机器 —— id 里混进 `@` 或主机名
    /// 就等于把令牌发给别人。这里不接受任何转义或编码，出错就直说。
    fn url_for(&self, id: Option<&str>) -> Result<String, ProviderError> {
        let root = match self.provider {
            ProviderKind::GitHub => format!("{GITHUB_API}/gists"),
            // id 的字符白名单对两家的形式都够用（GitHub 是十六进制串，Gitee 实测是 23 位小写字母数字）
            ProviderKind::Gitee => format!("{GITEE_API}/gists"),
        };
        match id {
            None => Ok(root),
            Some(id) => {
                if !is_valid_id(id) {
                    return Err(ProviderError::BadId);
                }
                Ok(format!("{root}/{id}"))
            }
        }
    }
}

/// id 的字符白名单。长度上限按 GitHub 的 sha 形式放宽到 128。
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// 白名单的对外入口：设置面板在用户粘完就当场拒掉明显不合法的串，
/// 而不是等他点「立即同步」、请求拼进地址之后才报一个看不出所以然的错。
pub fn is_valid_fragment_id(id: &str) -> bool {
    is_valid_id(id)
}

/// 编码：URL-safe 无填充。`-_` 在双击选中时不会被截断，也不含 `=`，
/// 用户把这个文件从网页上复制下来比对时不会因换行而变形。
fn encode(blob: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(blob)
}

/// 解码：接受我们自己写出去的形态，也接受标准 base64 与网页保存时插入的换行。
/// 宽容只用在读的一侧 —— 写出去的一律是 `encode` 那种，免得两份数据形状不一致。
fn decode(text: &str) -> Result<Vec<u8>, ProviderError> {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    if cleaned.is_empty() {
        return Err(ProviderError::UnexpectedShape);
    }
    let padded = match cleaned.len() % 4 {
        2 => format!("{cleaned}=="),
        3 => format!("{cleaned}="),
        0 => cleaned,
        // 长度 % 4 == 1 不可能是合法 base64，直接算「不是我们写出去的文件」
        _ => return Err(ProviderError::UnexpectedShape),
    };
    STANDARD.decode(&padded).map_err(|_| ProviderError::UnexpectedShape)
}

/// 创建时的请求体。两家都要 `files` 是「文件名 → 内容」的对象。
///
/// Gitee 这里有一段必须记住的实测：把 `filename`+`content` 平铺上去，创建直接被拒
/// （`files` is missing），而**更新**却回 200 且什么都不改。后者比 400 危险得多 ——
/// 客户端会以为推上去了，第二台设备永远停在老数据上。所以两家共用这一种形状，
/// 平铺那种写法在测试里被明确钉成「不能用」。
fn create_body(provider: ProviderKind, content: &str) -> serde_json::Value {
    match provider {
        ProviderKind::GitHub | ProviderKind::Gitee => serde_json::json!({
            "description": GIST_DESCRIPTION,
            // GitHub 上实测 false 挡不住匿名读，但它至少不让片段出现在用户的公开列表里；
            // Gitee 上 false 是真的挡得住（匿名读 API 与 raw 都是 403）
            "public": false,
            "files": { SYNC_FILE_NAME: { "content": content } },
        }),
    }
}

/// 更新时的请求体。创建要 `description` 与 `public`，更新只要内容 ——
/// 多余字段会被服务端原样改掉标题，而标题不该每次同步都重写一遍。
///
/// 两家的更新都用 `files` 哈希、都用 PATCH（Gitee 用 PUT 是 405），所以这里也共用。
fn update_body(provider: ProviderKind, content: &str) -> serde_json::Value {
    match provider {
        ProviderKind::GitHub | ProviderKind::Gitee => serde_json::json!({
            "files": { SYNC_FILE_NAME: { "content": content } },
        }),
    }
}

/// 从写入响应里取 id。创建与更新返回的是同一个形状。
fn parse_id(provider: ProviderKind, json: &serde_json::Value) -> Result<String, ProviderError> {
    match provider {
        ProviderKind::GitHub | ProviderKind::Gitee => json
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(ProviderError::UnexpectedShape),
    }
}

/// 解释读回来的整个片段对象。
///
/// 「文件不在」与「文件被截断」都必须与「读到了」分清楚：前者是正常的第一次同步，
/// 后者如果当成空数据处理，用户会得到一份悄悄少了一半的主机列表还毫不知情。
fn parse_cloud(provider: ProviderKind, json: &serde_json::Value) -> Result<Option<CloudBlob>, ProviderError> {
    // 2xx 但响应体是空的：这不是「云上还没有」，而是接口没按预期回东西
    if json.is_null() {
        return Err(ProviderError::UnexpectedShape);
    }
    // 两家的 `files` 都是「文件名 → 文件对象」的哈希，文件对象里都带 `content`
    // 与 `truncated`（实测字段清单：size, raw_url, type, truncated, content）。
    let file = match provider {
        ProviderKind::GitHub | ProviderKind::Gitee => json.get("files").and_then(|f| f.get(SYNC_FILE_NAME)),
    };
    let Some(file) = file else { return Ok(None) };
    if file.get("truncated").and_then(|t| t.as_bool()) == Some(true) {
        return Err(ProviderError::Truncated);
    }
    // Gitee 在整个片段对象上也带一个 `truncated`。它的确切语义没测出来（不知道是
    // 「某个文件截断」还是「整个响应截断」），而这两种都可能让我们在只拿到半份
    // 密文时毫不知情 —— 宁可在这里硬失败，也不能拿一份残缺的信封去解出半个列表。
    if provider == ProviderKind::Gitee && json.get("truncated").and_then(|t| t.as_bool()) == Some(true) {
        return Err(ProviderError::Truncated);
    }
    let content = file.get("content").and_then(|c| c.as_str()).ok_or(ProviderError::UnexpectedShape)?;
    let updated_at = match provider {
        ProviderKind::GitHub | ProviderKind::Gitee => {
            json.get("updated_at").and_then(|v| v.as_str()).map(|s| s.to_string())
        }
    };
    Ok(Some(CloudBlob { bytes: decode(content)?, updated_at }))
}

/// 带上 id 去读却收到 404，含义是「这个片段没了」，不是「云上还没有」。
/// 区别要说出来：前者要重建一份并把新地址告诉另一台设备，后者只需要按下同步按钮。
fn gone_as_missing(e: WireError) -> ProviderError {
    match e {
        WireError::NotFound => ProviderError::Missing,
        other => ProviderError::Wire(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Response;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// 进程内的假传输：记下每个发出去的请求，按脚本返回响应。
    /// 有了它，两家的 URL、请求体形状、各种状态码与字段缺失全部能离线钉住。
    #[derive(Clone, Default)]
    struct Fake {
        /// 待发响应，按顺序取；取完还来请求就是测试写漏了
        queued: Arc<Mutex<VecDeque<Result<Response, WireError>>>>,
        sent: Arc<Mutex<Vec<Sent>>>,
    }

    /// 记下来的请求。刻意**不**存令牌本身，只存「有没有」和长度：
    /// 断言「令牌没进 URL」时，测试代码自己也不该复制出一份令牌。
    struct Sent {
        method: Method,
        url: String,
        has_token: bool,
        body: Option<serde_json::Value>,
    }

    impl Fake {
        fn new() -> Self {
            Self::default()
        }

        fn ok(&self, json: serde_json::Value) {
            self.queued.lock().unwrap().push_back(Ok(Response { status: 200, json }));
        }

        fn status(&self, err: WireError) {
            self.queued.lock().unwrap().push_back(Err(err));
        }

        fn take(&self) -> Sent {
            self.sent.lock().unwrap().remove(0)
        }

        fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    impl Transport for Fake {
        fn send(&self, req: Request) -> crate::wire::BoxFuture<Result<Response, WireError>> {
            let sent = Sent {
                method: req.method,
                url: req.url.clone(),
                has_token: !req.token.is_empty(),
                body: req.body.clone(),
            };
            self.sent.lock().unwrap().push(sent);
            let next = self.queued.lock().unwrap().pop_front();
            Box::pin(async move {
                match next {
                    Some(r) => r,
                    None => panic!("脚本里没有更多响应了，说明多发了请求"),
                }
            })
        }
    }

    fn token() -> Zeroizing<String> {
        Zeroizing::new("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string())
    }

    fn client() -> SyncClient {
        SyncClient::new(ProviderKind::GitHub, None, token())
    }

    fn gitee(id: Option<&str>) -> SyncClient {
        SyncClient::new(ProviderKind::Gitee, id.map(|s| s.to_string()), token())
    }

    /// 按 Gitee 实测的响应形状拼一份（顶层多出来的 `owner/user/git_pull_url/truncated`
    /// 和文件对象里的 `size/type/raw_url` 都照抄进来）。
    /// 测试用真实形状而不是最小形状，才挡得住「我们只测了自己想当然的那几个字段」。
    fn gitee_json(content: &str) -> serde_json::Value {
        serde_json::json!({
            "url": "https://gitee.com/api/v5/gists/3d0rmjto24pfie6bawxsu73",
            "forks_url": "https://gitee.com/api/v5/gists/3d0rmjto24pfie6bawxsu73/forks",
            "commits_url": "https://gitee.com/api/v5/gists/3d0rmjto24pfie6bawxsu73/commits",
            "id": "3d0rmjto24pfie6bawxsu73",
            "description": GIST_DESCRIPTION,
            "public": false,
            "owner": { "id": 1, "login": "someone" },
            "user": { "id": 1, "login": "someone" },
            "files": {
                SYNC_FILE_NAME: {
                    "size": content.len(),
                    "raw_url": "https://gitee.com/gists/3d0rmjto24pfie6bawxsu73/raw",
                    "type": "json",
                    "truncated": false,
                    "content": content,
                }
            },
            "truncated": false,
            "html_url": "https://gitee.com/gists/3d0rmjto24pfie6bawxsu73",
            "comments": 0,
            "comments_url": "https://gitee.com/api/v5/gists/3d0rmjto24pfie6bawxsu73/comments",
            "git_pull_url": "https://gitee.com/3d0rmjto24pfie6bawxsu73.git",
            "git_push_url": "https://gitee.com/3d0rmjto24pfie6bawxsu73.git",
            "created_at": "2026-10-16T10:00:00+08:00",
            "updated_at": "2026-10-16T10:00:00+08:00",
            "forks": 0,
            "history": [],
        })
    }

    /// Gitee 那一家的核心回归：**更新用的是 `files` 哈希，不是平铺的 `filename`+`content`**。
    ///
    /// 平铺那种写法服务端回 200、响应体长得跟成功一模一样，可云端那份内容一个字都不改
    /// （实测：写 39 字节、推 439 字节、读回来还是 39 字节）。这种失败不会报错，
    /// 只会让第二台设备永远停在老数据上，所以把它钉在最显眼的一条测试里。
    #[tokio::test]
    async fn gitee_updates_through_the_files_hash_never_the_flat_fields() {
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "3d0rmjto24pfie6bawxsu73" }));
        let c = gitee(Some("3d0rmjto24pfie6bawxsu73"));
        c.push(&fake, b"payload").await.unwrap();

        let sent = fake.take();
        assert_eq!(sent.method, Method::Patch);
        assert_eq!(sent.url, "https://gitee.com/api/v5/gists/3d0rmjto24pfie6bawxsu73");
        assert!(!sent.url.contains("ghp_"), "令牌出现在 URL 里: {}", sent.url);
        let body = sent.body.unwrap();
        assert_eq!(decode(body["files"][SYNC_FILE_NAME]["content"].as_str().unwrap()).unwrap(), b"payload");
        assert!(body.get("filename").is_none(), "平铺的 filename 会静默不生效，绝不能发出去");
        assert!(body.get("content").is_none(), "同上：平铺的 content 是那种「回 200 但没写」的写法");
        // 更新不该重写标题：Gitee 的 description 同样会被原样改掉
        assert!(body.get("description").is_none());
        assert!(body.get("public").is_none());
    }

    #[tokio::test]
    async fn gitee_creates_through_the_same_files_hash_and_stays_private() {
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "3d0rmjto24pfie6bawxsu73", "public": false }));
        let target = gitee(None).push(&fake, b"\x01\x02\xff\xfe").await.unwrap();
        assert_eq!(target.provider, ProviderKind::Gitee);
        assert_eq!(target.id, "3d0rmjto24pfie6bawxsu73");

        let sent = fake.take();
        assert_eq!(sent.method, Method::Post);
        assert_eq!(sent.url, "https://gitee.com/api/v5/gists");
        assert!(!sent.url.contains("access_token"), "令牌不能靠 query: {}", sent.url);
        let body = sent.body.unwrap();
        assert_eq!(body["public"], serde_json::json!(false));
        assert_eq!(body["description"], serde_json::json!(GIST_DESCRIPTION));
        assert_eq!(decode(body["files"][SYNC_FILE_NAME]["content"].as_str().unwrap()).unwrap(), vec![1, 2, 0xff, 0xfe]);
    }

    #[tokio::test]
    async fn gitees_real_response_shape_round_trips() {
        let blob: Vec<u8> = (0..512u32).map(|i| (i * 251) as u8).collect();
        let fake = Fake::new();
        fake.ok(gitee_json(&encode(&blob)));
        let got = gitee(Some("3d0rmjto24pfie6bawxsu73")).fetch(&fake).await.unwrap().expect("该读到内容");
        assert_eq!(got.bytes, blob);
        assert_eq!(got.updated_at.as_deref(), Some("2026-10-16T10:00:00+08:00"));
        assert_eq!(fake.take().method, Method::Get);
    }

    #[tokio::test]
    async fn either_truncated_flag_on_gitee_is_a_hard_error() {
        // 文件级与片段级各来一次：两家只有 Gitee 在顶层也带这个标记，
        // 而它的语义没测出来，那就哪一种为真都按「拿不到完整数据」处理
        for level in ["file", "gist"] {
            let mut json = gitee_json("nothing");
            if level == "file" {
                json["files"][SYNC_FILE_NAME]["truncated"] = serde_json::json!(true);
                json["files"][SYNC_FILE_NAME]["content"] = serde_json::Value::Null;
            } else {
                json["truncated"] = serde_json::json!(true);
            }
            let fake = Fake::new();
            fake.ok(json);
            let err = gitee(Some("x")).fetch(&fake).await.unwrap_err();
            assert!(matches!(err, ProviderError::Truncated), "{level} 这一档放过了: {err:?}");
        }
        // GitHub 没有顶层那个字段，缺字段不能被当成「截断了」
        let fake = Fake::new();
        fake.ok(serde_json::json!({
            "id": "x", "files": { SYNC_FILE_NAME: { "content": encode(b"ok"), "truncated": false } }
        }));
        let c = SyncClient::new(ProviderKind::GitHub, Some("x".into()), token());
        assert_eq!(c.fetch(&fake).await.unwrap().unwrap().bytes, b"ok");
    }

    /// 两家的地址不能串：串了就是把一台的令牌发给另一台，而且还会读到「没有这个片段」。
    #[tokio::test]
    async fn each_provider_gets_its_own_host_and_never_the_other_s() {
        for (kind, host, path) in [
            (ProviderKind::GitHub, "api.github.com", "/gists/abc"),
            (ProviderKind::Gitee, "gitee.com", "/api/v5/gists/abc"),
        ] {
            let fake = Fake::new();
            fake.ok(gitee_json("x"));
            let c = SyncClient::new(kind, Some("abc".into()), token());
            let _ = c.fetch(&fake).await;
            let sent = fake.take();
            assert!(sent.url.contains(host) && sent.url.ends_with(path), "{kind:?} 发去了 {}", sent.url);
        }
    }

    #[tokio::test]
    async fn a_gitee_id_with_the_wrong_shape_is_refused_before_it_reaches_the_url() {
        // Gitee 的 id 实测是 23 位小写字母数字，白名单比它宽；宽一点没关系，
        // 但带 `/`、`?` 或 `@` 的粘贴内容必须先挡住，否则 header 里的令牌跟着跑了
        for bad in ["..%2f", "a/b", "x?access_token=1", "evil.com/api/v5/gists"] {
            let fake = Fake::new();
            let c = gitee(Some(bad));
            assert!(matches!(c.fetch(&fake).await.unwrap_err(), ProviderError::BadId), "放过了 {bad:?}");
            assert_eq!(fake.count(), 0);
        }
    }

    fn gist_json(content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "abc123",
            "updated_at": "2026-10-16T00:00:00Z",
            "files": { SYNC_FILE_NAME: { "content": content, "truncated": false, "size": content.len() } }
        })
    }

    #[tokio::test]
    async fn first_push_creates_a_private_gist_and_returns_its_id() {
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "deadbeef" }));
        let target = client().push(&fake, b"\x01\x02\xff\xfe").await.unwrap();
        assert_eq!(target.id, "deadbeef");
        assert_eq!(target.provider, ProviderKind::GitHub);

        let sent = fake.take();
        assert_eq!(sent.method, Method::Post);
        assert_eq!(sent.url, "https://api.github.com/gists");
        assert!(!sent.url.contains("ghp_"), "令牌出现在 URL 里: {}", sent.url);
        assert!(sent.has_token, "令牌没交给传输层");

        let body = sent.body.expect("创建必须带请求体");
        assert_eq!(body["public"], serde_json::json!(false));
        assert_eq!(body["description"], serde_json::json!(GIST_DESCRIPTION));
        // 任意字节都得先变成合法的 JSON 字符串；这里钉的是「不是原文」
        assert_ne!(body["files"][SYNC_FILE_NAME]["content"], serde_json::json!("\x01\x02"));
        assert_eq!(decode(body["files"][SYNC_FILE_NAME]["content"].as_str().unwrap()).unwrap(), vec![1, 2, 0xff, 0xfe]);
    }

    #[tokio::test]
    async fn a_known_target_updates_in_place_instead_of_creating_another_gist() {
        // 这一条守的是最贵的那种事故：每次同步多建一份，第二台设备不知道该读哪份
        let c = SyncClient::new(ProviderKind::GitHub, Some("deadbeef".into()), token());
        assert_eq!(c.target().unwrap().id, "deadbeef");
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "deadbeef" }));
        c.push(&fake, b"payload").await.unwrap();
        let sent = fake.take();
        assert_eq!(sent.method, Method::Patch);
        assert_eq!(sent.url, "https://api.github.com/gists/deadbeef");
        let body = sent.body.unwrap();
        // 更新不该重写标题：多余字段会被 GitHub 真的改掉
        assert!(body.get("description").is_none());
        assert!(body.get("public").is_none());
        assert_eq!(decode(body["files"][SYNC_FILE_NAME]["content"].as_str().unwrap()).unwrap(), b"payload");
    }

    #[tokio::test]
    async fn fetching_without_a_target_does_not_touch_the_network() {
        let fake = Fake::new();
        assert!(client().fetch(&fake).await.unwrap().is_none());
        assert_eq!(fake.count(), 0, "没有 id 就没得读，不该发请求");
    }

    #[tokio::test]
    async fn read_back_the_same_bytes_that_were_written() {
        let fake = Fake::new();
        // 二进制信封走一遍完整往返：编码 → 云端存的字符串 → 解码
        let blob: Vec<u8> = (0..1000u32).map(|i| (i * 37) as u8).collect();
        fake.ok(gist_json(&encode(&blob)));
        let c = SyncClient::new(ProviderKind::GitHub, Some("deadbeef".into()), token());
        let got = c.fetch(&fake).await.unwrap().expect("该读到内容");
        assert_eq!(got.bytes, blob);
        assert_eq!(got.updated_at.as_deref(), Some("2026-10-16T00:00:00Z"));
        assert_eq!(fake.take().method, Method::Get);
    }

    #[tokio::test]
    async fn a_gist_without_our_file_is_absent_not_an_error() {
        // 用户在网页上新建了个同名片段，或把我们那个文件删了：
        // 对同步来说这是「云上还没有」，push 会把它补回去
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "x", "files": { "别的文件.txt": { "content": "hi" } } }));
        let c = SyncClient::new(ProviderKind::GitHub, Some("x".into()), token());
        assert!(c.fetch(&fake).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_truncated_file_is_a_hard_error_never_an_empty_sync() {
        // 这一条是整个 provider 里最值钱的断言：把它当成「没有数据」，
        // 用户会得到一份悄悄少了一半的列表，还会把这份残缺推回云端
        let fake = Fake::new();
        fake.ok(serde_json::json!({
            "id": "x",
            "files": { SYNC_FILE_NAME: { "truncated": true, "size": 5_000_000 } }
        }));
        let c = SyncClient::new(ProviderKind::GitHub, Some("x".into()), token());
        let err = c.fetch(&fake).await.unwrap_err();
        assert!(matches!(err, ProviderError::Truncated), "{err:?}");
        let ipc: IpcError = err.into();
        assert_eq!(ipc.code, ErrorCode::QuotaExceeded);
        assert!(!ipc.retryable, "再点一次「同步」不会让它变小");
    }

    #[tokio::test]
    async fn oversize_payload_is_refused_before_any_request_goes_out() {
        let fake = Fake::new();
        let big = vec![7u8; MAX_BLOB_BYTES + 1];
        let err = client().push(&fake, &big).await.unwrap_err();
        assert!(matches!(err, ProviderError::TooLarge { .. }), "{err:?}");
        assert_eq!(fake.count(), 0, "超预算的数据不该被发到云上");
    }

    #[tokio::test]
    async fn a_deleted_gist_reports_the_target_is_missing_so_the_ui_can_recreate_it() {
        let fake = Fake::new();
        fake.status(WireError::NotFound);
        let c = SyncClient::new(ProviderKind::GitHub, Some("gone".into()), token());
        let err = c.fetch(&fake).await.unwrap_err();
        assert!(matches!(err, ProviderError::Missing), "{err:?}");
        // 带着 id 却 404 是确凿的「云上没了」，文案要能让用户明白下一步是重建
        let ipc: IpcError = err.into();
        assert_eq!(ipc.code, ErrorCode::NotFound);
        // 已经没了的再删一次算成功：用户要的结局达成了
        let fake2 = Fake::new();
        fake2.status(WireError::NotFound);
        assert!(c.delete(&fake2).await.is_ok());
    }

    #[tokio::test]
    async fn a_response_missing_expected_fields_is_shaped_as_api_change_not_bad_key() {
        let fake = Fake::new();
        fake.ok(serde_json::json!({ "id": "x", "files": { SYNC_FILE_NAME: { "truncated": false } } }));
        let c = SyncClient::new(ProviderKind::GitHub, Some("x".into()), token());
        assert!(matches!(c.fetch(&fake).await.unwrap_err(), ProviderError::UnexpectedShape));

        let fake2 = Fake::new();
        fake2.ok(serde_json::json!({ "files": {} }));
        assert!(matches!(client().push(&fake2, b"x").await.unwrap_err(), ProviderError::UnexpectedShape));
    }

    #[tokio::test]
    async fn a_pasted_id_that_could_point_elsewhere_is_rejected() {
        // id 会拼进地址，而 header 里的令牌跟着地址走：白名单不是洁癖
        let long = "0123456789".repeat(13);
        for bad in ["../../evil", "@evil.com", "a b", "x?y=1", "x#f", long.as_str()] {
            let c = SyncClient::new(ProviderKind::GitHub, Some(bad.to_string()), token());
            let fake = Fake::new();
            assert!(matches!(c.fetch(&fake).await.unwrap_err(), ProviderError::BadId), "放过了 {bad:?}");
            assert!(matches!(c.push(&fake, b"x").await.unwrap_err(), ProviderError::BadId), "推送放过了 {bad:?}");
            assert_eq!(fake.count(), 0, "{bad:?} 该在发请求前就被拦下");
        }
        // 空白 id 是「还没配」而不是「配了个坏值」：走 fetch 的 Ok(None)，不发请求
        let fake = Fake::new();
        let unset = SyncClient::new(ProviderKind::GitHub, Some("   ".into()), token());
        assert!(unset.fetch(&fake).await.unwrap().is_none());
        assert!(unset.target().is_none());
        assert_eq!(fake.count(), 0);
    }

    #[test]
    fn base64_survives_both_alphabets_and_the_web_editors_newlines() {
        let raw: Vec<u8> = vec![0xfb, 0xff, 0x00, 0x7f, 0x2b, 0x2f];
        let ours = encode(&raw);
        assert_eq!(decode(&ours).unwrap(), raw);
        // 有人在网页上改成了标准字母表、还带上了换行与填充
        let standard = STANDARD.encode(&raw);
        let noisy = standard.as_str().replace('\n', "");
        assert_eq!(decode(&format!("{}\r\n", noisy)).unwrap(), raw);
        // 不是我们写出去的文件要报错，而不是「同步到了一份空列表」
        for junk in ["", "   ", "!!!!", "abc=f"] {
            assert!(decode(junk).is_err(), "{junk:?} 居然被解开了");
        }
    }

    #[test]
    fn client_debug_never_prints_the_token() {
        let c = SyncClient::new(ProviderKind::GitHub, Some("deadbeef".into()), token());
        let rendered = format!("{c:?}");
        assert!(rendered.contains("deadbeef"), "id 不是秘密，排障要看: {rendered}");
        assert!(!rendered.contains("ghp_AAAAAA"), "Debug 泄露了令牌: {rendered}");
        assert!(rendered.contains("redacted"));
    }

    /// 真联网的用例，默认不跑。上面所有离线测试证明的都只是
    /// 「我们以为接口是这样」，而接口只有远端说了算。
    ///
    /// 跑法：`DBHX_TOKEN_FILE=<令牌文件> cargo test -p dbhx-sync -- --ignored --nocapture`。
    /// 它建一个片段、写一次、读回来比对、**测完删掉**；
    /// 令牌只从仓库外的文件读，任何输出都不带它。
    #[tokio::test]
    #[ignore = "需要真实令牌，手动跑"]
    async fn talks_to_the_real_github_api_and_leaves_nothing_behind() {
        let token = crate::token_from_file();

        // 一段真信封形状的字节：含非 UTF-8，用来证明 base64 那一层不是多余的
        let probe: Vec<u8> = b"DBHXSYN1".iter().copied().chain((0u8..64).map(|i| i.wrapping_mul(37))).collect();
        let transport = crate::wire::HttpTransport::new();
        let fresh = SyncClient::new(ProviderKind::GitHub, None, token.clone());
        let target = fresh.push(&transport, &probe).await.expect("创建片段失败");

        let with_id = SyncClient::from_target(&target, token.clone());
        let read_back = with_id.fetch(&transport).await.expect("读回失败").expect("刚写完就读不到，说明写入没生效");
        assert_eq!(read_back.bytes, probe, "读回来的内容与写进去的不一致");

        // 再推一次：必须原地更新，云上不能多出第二份
        let again = with_id.push(&transport, &probe).await.expect("更新失败");
        assert_eq!(again.id, target.id, "更新变成了新建");

        with_id.delete(&transport).await.expect("清理失败：请手动删除标题含 dbhx-shell 的片段");
        assert!(with_id.fetch(&transport).await.is_err(), "删完还在，说明没真删");
    }

    /// Gitee 这一条必须**换一段长度不同的内容**再读一次：状态码 200 在这一家
    /// 证明不了写进去了 —— 实测平铺 `filename`+`content` 就是 200、响应漂亮、
    /// 云端一个字没改。离线测试全绿而用户换设备拉到老数据，就是这种事故。
    ///
    /// 中间步骤一律用 `?` 而不是 `expect`： panic 会跳过删除那一步，把密文留在云上。
    #[tokio::test]
    #[ignore = "需要真实令牌，手动跑"]
    async fn talks_to_the_real_gitee_api_and_leaves_nothing_behind() {
        let token = crate::token_from_file();
        let first: Vec<u8> = b"DBHXSYN1".iter().copied().chain((0u8..64).map(|i| i.wrapping_mul(37))).collect();
        // 长度不同才分辨得出读回来的是新那份还是老那份
        let second: Vec<u8> = b"DBHXSYN2".iter().copied().chain((0u8..200).map(|i| i.wrapping_mul(131))).collect();
        let transport = crate::wire::HttpTransport::new();
        let fresh = SyncClient::new(ProviderKind::Gitee, None, token.clone());
        let target = fresh.push(&transport, &first).await.expect("创建片段失败");
        let with_id = SyncClient::from_target(&target, token.clone());

        let outcome = async {
            let read_back = with_id
                .fetch(&transport)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "刚写完就读不到，说明创建没生效".to_string())?;
            if read_back.bytes != first {
                return Err("创建后读回来的内容与写进去的不一致".to_string());
            }
            let again = with_id.push(&transport, &second).await.map_err(|e| e.to_string())?;
            if again.id != target.id {
                return Err(format!("更新变成了新建：{} 变成了 {}", target.id, again.id));
            }
            let updated = with_id
                .fetch(&transport)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "更新后读不到".to_string())?;
            // 这一句是整个用例的意义所在
            if updated.bytes != second {
                return Err("更新回 200 却没真改掉云端那份 —— 请求体的字段形状不对就是这个表现".to_string());
            }
            Ok::<(), String>(())
        }
        .await;

        // 走到这里才删：上面无论成功还是失败，云上那份都不该留下
        let cleanup = with_id.delete(&transport).await;
        let gone = with_id.fetch(&transport).await;
        if let Err(msg) = outcome {
            panic!("{msg}（片段 id {}，清理 {:?}）", target.id, cleanup);
        }
        cleanup.expect("清理失败：请手动删除标题含 dbhx-shell 的那个片段");
        assert!(matches!(gone, Err(ProviderError::Missing)), "删完还在，说明没真删");
    }
}
