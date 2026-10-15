//! transport 池：身份键 ⇒ 一条已认证的连接，lease 引用计数决定它的生死。
//!
//! ## 四条规则，缺一不可
//!
//! 1. **命中即零凭据**。查到可复用的连接就直接返回，绝不问 `PlanSource`——这既是
//!    「免二次登录」，也是明文不过第二次手。
//! 2. **同一身份键的并发拨号只发生一次**。靠每键一把异步锁，不是靠「拨得快」：
//!    同时开三个 tab 触发三次认证，开了 OTP 的服务器就会弹三次码，用户无法判断哪次是真的。
//! 3. **最后一个 lease 释放 ⇒ 进入 idle park，而不是立刻断开**。断开时机由 TTL 决定。
//! 4. **拨号锁是每键的，条目锁是全局但极短的**。`entries` 用 `std::sync::Mutex` 且
//!    **绝不跨 `await` 持有**（编译器会拦住，因为 guard 不是 `Send`），这样 A 主机慢拨号
//!    不会拖住 B 主机。
//!
//! ## 生命周期
//!
//! 连接的实际寿命由 `Arc` 句柄决定：池 drop 不等于断开（supervisor 任务还握着句柄）。
//! 需要在退出前主动断开就调 [`TransportPool::shutdown`]。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use dbhx_model::LiveEndpoint;
use tokio::time::Instant;

use crate::conn::{Dialer, PooledConnection, SharedConnection};
use crate::error::SessionError;
use crate::key::TransportKey;
use crate::plan::PlanSource;

/// lease 的用途。计数只看总数，用途只进日志——排查「这条连接为什么还不断」时，
/// 需要能看出压着它的是终端还是文件面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseKind {
    Shell,
    Sftp,
    /// 连上后借道跑一条 exec 的系统探测（见 `dbhx-ipc` 的 `osdetect`）。几毫秒就还。
    Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolOptions {
    /// 最后一个 lease 释放后多久断开。`Duration::ZERO` = 一直保留到进程退出。
    ///
    /// 默认 5 分钟：短于用户「关掉 tab 又马上想回来」的间隔，长到足以跨过网络抖动重连。
    pub idle_ttl: Duration,
    /// 空闲连接上限，超出按「最早空闲」淘汰。正在被 lease 占用的不参与淘汰。
    pub max_idle: usize,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self { idle_ttl: Duration::from_secs(5 * 60), max_idle: 128 }
    }
}

/// 一条连接在池里的样子。`leases == 0` 时 `idle_since` 必为 `Some`。
struct Entry {
    conn: SharedConnection,
    leases: usize,
    idle_since: Option<Instant>,
}

impl Entry {
    fn hold(&mut self) {
        self.leases += 1;
        self.idle_since = None;
    }

    /// 归还一次引用；返回是否因此进入空闲。
    fn release(&mut self) -> bool {
        self.leases = self.leases.saturating_sub(1);
        if self.leases == 0 {
            self.idle_since = Some(Instant::now());
            return true;
        }
        false
    }

    fn idle(&self) -> bool {
        self.leases == 0
    }
}

pub struct TransportPool {
    inner: Arc<Inner>,
}

impl TransportPool {
    pub fn new(dialer: Dialer, plans: PlanSource, options: PoolOptions) -> Self {
        Self { inner: Arc::new(Inner::new(dialer, plans, options)) }
    }

    pub async fn acquire(&self, key: TransportKey, kind: LeaseKind) -> Result<Lease, SessionError> {
        self.inner.clone().acquire(key, kind).await
    }

    /// 关掉并清空所有连接。退出流程用。
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    /// 池中的连接总数（含空闲）。诊断与测试用。
    pub fn live_count(&self) -> usize {
        self.inner.entries().len()
    }

    /// 空闲连接数，即 `lease == 0` 的条目数。
    pub fn idle_count(&self) -> usize {
        self.inner.entries().values().filter(|e| e.idle()).count()
    }
}

struct Inner {
    dialer: Dialer,
    plans: PlanSource,
    options: PoolOptions,
    entries: Mutex<HashMap<TransportKey, Entry>>,
    /// 每键一把拨号锁。存 `Weak` 是为了让等的人散尽后自动消失——锁表不能随历史主机数增长。
    slots: Mutex<HashMap<TransportKey, Weak<Slot>>>,
    /// 是否已有一个清扫任务在跑。见 `arm` / `disarm_if_still_idle` 的交接。
    sweep_armed: AtomicBool,
}

type Slot = tokio::sync::Mutex<()>;

impl Inner {
    fn new(dialer: Dialer, plans: PlanSource, options: PoolOptions) -> Self {
        Self {
            dialer,
            plans,
            options,
            entries: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
            sweep_armed: AtomicBool::new(false),
        }
    }

    /// `std::sync::Mutex` 中毒时继续用而不是 panic：`Lease::drop` 里 panic 会把退出流程
    /// 带崩，而池的状态最差只是「某条连接多活一会儿」，不会把秘密写到错的地方。
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<TransportKey, Entry>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<TransportKey, Weak<Slot>>> {
        self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn acquire(self: Arc<Self>, key: TransportKey, kind: LeaseKind) -> Result<Lease, SessionError> {
        if let Some(lease) = self.try_reuse(&key, kind) {
            return Ok(lease);
        }

        let slot = self.dial_slot(&key);
        let _guard = slot.lock().await;

        // 排队期间可能已经有人拨好了。这一句是整个去重逻辑的全部意义。
        if let Some(lease) = self.try_reuse(&key, kind) {
            return Ok(lease);
        }

        let plan = (self.plans)(key.clone()).await?;
        let conn = (self.dialer)(key.clone(), plan).await?;
        if conn.is_closed() {
            // 拨完立刻被对端关掉（协议层之外的拒绝）。不入库，直接回报，
            // 免得一条死连接占着身份键让后来的请求误以为可以复用。
            return Err(SessionError::Closed);
        }
        Ok(self.insert_and_lease(key, conn, kind).await)
    }

    fn try_reuse(self: &Arc<Self>, key: &TransportKey, kind: LeaseKind) -> Option<Lease> {
        let mut map = self.entries();
        let entry = map.get_mut(key)?;
        if entry.conn.is_closed() {
            let endpoint = entry.conn.endpoint().clone();
            map.remove(key);
            // supervisor 通常已经先摘掉了；走到这里说明是竞态兜底。
            tracing::info!(host = %endpoint.host, "池中连接已断开，摘除条目");
            return None;
        }
        entry.hold();
        Some(Lease { pool: Arc::clone(self), key: key.clone(), kind, conn: Arc::clone(&entry.conn) })
    }

    async fn insert_and_lease(
        self: &Arc<Self>,
        key: TransportKey,
        conn: SharedConnection,
        kind: LeaseKind,
    ) -> Lease {
        {
            let mut map = self.entries();
            map.insert(key.clone(), Entry { conn: Arc::clone(&conn), leases: 1, idle_since: None });
        }
        self.supervise(key.clone(), Arc::clone(&conn));
        // 超预算时挤掉最旧的空闲连接。刚插入的这条 leases=1，不会被自己挤掉。
        let evicted = self.enforce_idle_cap();
        for dead in evicted {
            dead.close().await;
        }
        Lease { pool: Arc::clone(self), key, kind, conn }
    }

    /// 每条连接一个 supervisor 任务：远端断开 ⇒ 摘除条目。
    ///
    /// 任务用 `Weak<Inner>` 而不是强引用池，为的是「池被释放」时它能自己结束。
    /// 代价是任务期间它握着那一条连接，所以池 drop 不等于断开——见模块文档的寿命说明。
    fn supervise(self: &Arc<Self>, key: TransportKey, conn: SharedConnection) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let reason = conn.wait_closed().await;
            let host = conn.endpoint().host.clone();
            match &weak.upgrade() {
                Some(inner) => {
                    // 只摘自己那一条：同一身份键可能已经重连过，误删会把新连接留成孤儿
                    if inner.remove_if_same(&key, &conn) {
                        tracing::info!(%host, ?reason, "池中连接结束，条目已摘除");
                    } else {
                        tracing::debug!(%host, ?reason, "池中连接结束，条目已被别处摘除");
                    }
                }
                // 池已释放，条目无处可摘；连接句柄在这里 drop
                None => tracing::debug!(%host, ?reason, "连接结束，池已释放"),
            }
        });
    }

    fn remove_if_same(&self, key: &TransportKey, conn: &SharedConnection) -> bool {
        let mut map = self.entries();
        let same = map.get(key).is_some_and(|entry| Arc::ptr_eq(&entry.conn, conn));
        same && map.remove(key).is_some()
    }

    fn dial_slot(&self, key: &TransportKey) -> Arc<Slot> {
        let mut slots = self.slots();
        // 没人再等待的槽位已经完成使命。留着它们，锁表就会随历史主机数无限增长。
        slots.retain(|_, weak| weak.upgrade().is_some());
        if let Some(existing) = slots.get(key).and_then(Weak::upgrade) {
            return existing;
        }
        let slot = Arc::new(Slot::new(()));
        slots.insert(key.clone(), Arc::downgrade(&slot));
        slot
    }

    /// 释放一次引用。同步函数：`Lease::drop` 里调用，异步的收尾只能交给清扫任务。
    fn release(self: &Arc<Self>, key: &TransportKey, kind: LeaseKind) {
        let parked = {
            let mut map = self.entries();
            match map.get_mut(key) {
                // 连接已被摘除（远端断开或 TTL 淘汰），减无可减
                None => false,
                Some(entry) => entry.release(),
            }
        };
        if parked {
            tracing::debug!(host = %key.describe(), ?kind, "最后一个 lease 释放，进入 idle park");
            self.arm();
        }
    }

    /// 最早的空闲到期时刻还差多久。`None` 表示当前没有需要等的条目。
    fn next_idle_wait(&self) -> Option<Duration> {
        if self.options.idle_ttl.is_zero() {
            return None;
        }
        let map = self.entries();
        let earliest = map.values().filter_map(|e| e.idle_since).min()?;
        Some((earliest + self.options.idle_ttl).saturating_duration_since(Instant::now()))
    }

    /// 武装清扫任务。不变量：`sweep_armed == true` ⇔ 有一个任务在跑（或即将在
    /// `disarm_if_still_idle` 里退出）。
    fn arm(self: &Arc<Self>) {
        if self.options.idle_ttl.is_zero() {
            return;
        }
        if self.sweep_armed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(sweep(weak));
    }

    /// 与 `release` 用同一把 `entries` 锁交接标记，否则会出现「刚进入空闲却没人管」的丢唤醒。
    fn disarm_if_still_idle(&self) -> bool {
        let map = self.entries();
        if map.values().any(|e| e.idle_since.is_some()) {
            return false;
        }
        self.sweep_armed.store(false, Ordering::Release);
        true
    }

    /// TTL 到点的空闲连接。返回它们是为了让调用方在锁外 `close().await`。
    fn sweep_expired(&self) -> Vec<SharedConnection> {
        if self.options.idle_ttl.is_zero() {
            return Vec::new();
        }
        let ttl = self.options.idle_ttl;
        let now = Instant::now();
        let mut map = self.entries();
        let expired: Vec<TransportKey> = map
            .iter()
            .filter(|(_, e)| matches!(e.idle_since, Some(since) if now.saturating_duration_since(since) >= ttl))
            .map(|(key, _)| key.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|key| map.remove(&key))
            .map(|entry| entry.conn)
            .collect()
    }

    /// 空闲数量超预算 ⇒ 按最早空闲淘汰。正在用的连接不参与。
    fn enforce_idle_cap(&self) -> Vec<SharedConnection> {
        let mut map = self.entries();
        let mut idle: Vec<(Instant, TransportKey)> = map
            .iter()
            .filter(|(_, e)| e.idle())
            .filter_map(|(key, e)| e.idle_since.map(|since| (since, key.clone())))
            .collect();
        if idle.len() <= self.options.max_idle {
            return Vec::new();
        }
        idle.sort_by_key(|(since, _)| *since);
        let over = idle.len() - self.options.max_idle;
        idle.truncate(over);
        idle.into_iter()
            .filter_map(|(_, key)| map.remove(&key))
            .map(|entry| {
                tracing::info!(host = %entry.conn.endpoint().host, "空闲连接超出上限，提前淘汰");
                entry.conn
            })
            .collect()
    }

    async fn shutdown(self: &Arc<Self>) {
        let draining: Vec<SharedConnection> = {
            let mut map = self.entries();
            map.drain().map(|(_, entry)| entry.conn).collect()
        };
        for conn in draining {
            conn.close().await;
        }
    }
}

/// 清扫循环。睡着的时候不持有池的强引用，否则刚被 drop 的池会被这个任务钉住。
async fn sweep(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let Some(wait) = inner.next_idle_wait() else {
            let done = inner.disarm_if_still_idle();
            drop(inner);
            if done {
                return;
            }
            continue;
        };
        drop(inner);
        tokio::time::sleep(wait).await;
        let Some(inner) = weak.upgrade() else { return };
        for expired in inner.sweep_expired() {
            tracing::info!(host = %expired.endpoint().host, "idle park 到期，断开");
            expired.close().await;
        }
    }
}

/// 一次引用。drop 即归还；把它想象成「这个 tab 正在用这条连接」。
pub struct Lease {
    pool: Arc<Inner>,
    key: TransportKey,
    kind: LeaseKind,
    conn: SharedConnection,
}

impl Lease {
    pub fn key(&self) -> &TransportKey {
        &self.key
    }

    pub fn kind(&self) -> LeaseKind {
        self.kind
    }

    pub fn endpoint(&self) -> &LiveEndpoint {
        self.conn.endpoint()
    }

    pub fn connection(&self) -> &SharedConnection {
        &self.conn
    }

    /// 与另一个 lease 是否共用同一条 transport。测试与「免登录跳转」的判据。
    pub fn shares_transport_with(&self, other: &Lease) -> bool {
        Arc::ptr_eq(&self.conn, &other.conn)
    }
}

/// 直接当连接用：`lease.open_shell(...)`。
impl std::ops::Deref for Lease {
    type Target = dyn PooledConnection;

    fn deref(&self) -> &Self::Target {
        &*self.conn
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.release(&self.key, self.kind);
    }
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只打身份键与状态：连接对象本身可能带可回溯到凭据的信息
        f.debug_struct("Lease").field("key", &self.key).field("kind", &self.kind).field("state", &self.state()).finish()
    }
}

impl std::fmt::Debug for TransportPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.inner.entries();
        f.debug_struct("TransportPool")
            .field("live", &map.len())
            .field("idle", &map.values().filter(|e| e.idle()).count())
            .field("options", &self.inner.options)
            .finish()
    }
}
