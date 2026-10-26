//! transport 池的行为测试：去重、lease 计数、idle park、容量淘汰。
//!
//! 放在 `tests/` 而不是模块内，因为被验的四个机制全部是公开 API，而假连接需要
//! `PooledConnection` 的公开签名。时钟用 `start_paused`，TTL 相关的用例几毫秒跑完。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbhx_model::{AuthRef, Endpoint, HostId, LiveEndpoint, PtySize};
use dbhx_session::{
    BoxFuture, DialPlan, Dialer, Lease, LeaseKind, PlanSource, PoolOptions, PooledConnection,
    SharedConnection, TransportKey, TransportPool,
};
use dbhx_ssh::{
    wait_until_closed, CloseReason, Credential, Exec, SessionState, Shell, SshError,
    SubsystemStream, TrustAll,
};
use tokio::sync::watch;
use zeroize::Zeroizing;

/// 假连接：只实现池真正会用到的那几件事（状态、关闭、等关闭）。
struct FakeConn {
    endpoint: LiveEndpoint,
    state: Arc<watch::Sender<SessionState>>,
    closes: AtomicUsize,
}

impl FakeConn {
    fn new(endpoint: LiveEndpoint) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            state: Arc::new(watch::channel(SessionState::Authenticated).0),
            closes: AtomicUsize::new(0),
        })
    }

    /// 模拟远端 disconnect：supervisor 应当因此把条目摘掉。
    fn kill_remotely(&self) {
        let _ = self.state.send(SessionState::Closed(CloseReason::Server {
            reason_code: "byApplication".into(),
            message: "测试".into(),
        }));
    }

    fn close_count(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }
}

impl PooledConnection for FakeConn {
    fn endpoint(&self) -> &LiveEndpoint {
        &self.endpoint
    }

    fn state(&self) -> SessionState {
        self.state.borrow().clone()
    }

    fn open_shell(&self, _term: String, _pty: PtySize) -> BoxFuture<'_, Result<Shell, SshError>> {
        Box::pin(async { Err(SshError::Closed) })
    }

    fn open_exec(&self, _command: String) -> BoxFuture<'_, Result<Exec, SshError>> {
        Box::pin(async { Err(SshError::Closed) })
    }

    fn open_subsystem(
        &self,
        _name: String,
    ) -> BoxFuture<'_, Result<SubsystemStream, SshError>> {
        Box::pin(async { Err(SshError::Closed) })
    }

    fn open_sftp_elevated(
        &self,
        _sudo_user: String,
        _password: Zeroizing<String>,
    ) -> BoxFuture<'_, Result<SubsystemStream, SshError>> {
        Box::pin(async { Err(SshError::Closed) })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let _ = state.send(SessionState::Closed(CloseReason::ClientRequested));
        })
    }

    fn wait_closed(&self) -> BoxFuture<'_, CloseReason> {
        let mut rx = self.state.subscribe();
        Box::pin(async move { wait_until_closed(&mut rx).await })
    }
}

#[derive(Default)]
struct FakeFactory {
    dials: AtomicUsize,
    conns: Mutex<Vec<Arc<FakeConn>>>,
    /// 拖慢某个主机，用来验证「不同身份键不会被同一把锁串行化」
    slow: Mutex<HashMap<String, Duration>>,
    fail_times: AtomicUsize,
}

impl FakeFactory {
    fn dialer(self: &Arc<Self>) -> Dialer {
        let me = Arc::clone(self);
        Box::new(move |key, _plan| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.dials.fetch_add(1, Ordering::SeqCst);
                if me.fail_times.load(Ordering::SeqCst) > 0 {
                    me.fail_times.fetch_sub(1, Ordering::SeqCst);
                    return Err(dbhx_session::SessionError::Ssh(SshError::AuthFailed));
                }
                let wait = me.slow.lock().unwrap().get(&key.endpoint.host).copied();
                if let Some(wait) = wait {
                    tokio::time::sleep(wait).await;
                }
                let conn = FakeConn::new(key.endpoint.clone());
                me.conns.lock().unwrap().push(Arc::clone(&conn));
                Ok(conn as SharedConnection)
            })
        })
    }

    fn dial_count(&self) -> usize {
        self.dials.load(Ordering::SeqCst)
    }

    fn conn(&self, index: usize) -> Arc<FakeConn> {
        Arc::clone(&self.conns.lock().unwrap()[index])
    }

    fn last(&self) -> Arc<FakeConn> {
        let conns = self.conns.lock().unwrap();
        Arc::clone(conns.last().expect("还没拨过号"))
    }
}

fn plans(calls: Arc<AtomicUsize>) -> PlanSource {
    Box::new(move |_key| {
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(DialPlan {
                credential: Credential::Agent,
                verifier: Box::new(TrustAll),
                keepalive: None,
            })
        })
    })
}

fn endpoint(host: &str) -> LiveEndpoint {
    LiveEndpoint::from_endpoint(&Endpoint {
        host: host.into(),
        port: Some(22),
        username: "root".into(),
    })
}

fn key(host: &str) -> TransportKey {
    let id = HostId(format!("h-{host}"));
    TransportKey::new(id.clone(), endpoint(host), AuthRef::Host { host_id: id.0 }, 30)
}

struct Harness {
    pool: TransportPool,
    factory: Arc<FakeFactory>,
    plan_calls: Arc<AtomicUsize>,
}

impl Harness {
    fn new(options: PoolOptions) -> Self {
        let factory = Arc::new(FakeFactory::default());
        let plan_calls = Arc::new(AtomicUsize::new(0));
        let pool = TransportPool::new(factory.dialer(), plans(Arc::clone(&plan_calls)), options);
        Self { pool, factory, plan_calls }
    }

    fn dial_count(&self) -> usize {
        self.factory.dial_count()
    }

    fn plan_count(&self) -> usize {
        self.plan_calls.load(Ordering::SeqCst)
    }

    async fn acquire(&self, host: &str) -> Lease {
        self.pool.acquire(key(host), LeaseKind::Shell).await.expect("拨号应当成功")
    }
}

/// 推进时钟，并给清扫任务与 supervisor 任务跑完的机会。
async fn settle() {
    tokio::time::sleep(Duration::from_secs(3600)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn five_concurrent_acquires_of_one_host_dial_once() {
    let h = Harness::new(PoolOptions::default());
    let (a, b, c, d, e) = tokio::join!(
        h.pool.acquire(key("db1"), LeaseKind::Shell),
        h.pool.acquire(key("db1"), LeaseKind::Shell),
        h.pool.acquire(key("db1"), LeaseKind::Sftp),
        h.pool.acquire(key("db1"), LeaseKind::Shell),
        h.pool.acquire(key("db1"), LeaseKind::Sftp),
    );
    let leases = vec![
        a.expect("五个请求都该成功"),
        b.expect("五个请求都该成功"),
        c.expect("五个请求都该成功"),
        d.expect("五个请求都该成功"),
        e.expect("五个请求都该成功"),
    ];

    assert_eq!(h.dial_count(), 1, "五次认证就是两次 OTP 弹窗的根源");
    assert_eq!(h.plan_count(), 1, "凭据来源也只该被问一次");
    for pair in leases.windows(2) {
        assert!(pair[0].shares_transport_with(&pair[1]), "复用必须是同一条 transport");
    }
    assert_eq!(h.pool.idle_count(), 0);
}

#[tokio::test]
async fn reuse_never_touches_the_credential_source() {
    let h = Harness::new(PoolOptions::default());
    let first = h.acquire("db1").await;
    assert_eq!(h.plan_count(), 1);

    let second = h.acquire("db1").await;
    assert_eq!(h.plan_count(), 1, "命中池就不再要凭据：零凭据复用");
    assert_eq!(h.dial_count(), 1);
    assert!(first.shares_transport_with(&second));
}

#[tokio::test(start_paused = true)]
async fn different_hosts_are_not_serialized_behind_one_dial() {
    let h = Harness::new(PoolOptions::default());
    h.factory.slow.lock().unwrap().insert("slow".into(), Duration::from_secs(30));

    let started = tokio::time::Instant::now();
    // 两个请求并发发出：若池用了全局锁，fast 必须等满 30 秒
    let (slow, (fast_at, fast)) = tokio::join!(
        h.acquire("slow"),
        async {
            let lease = h.acquire("fast").await;
            (started.elapsed(), lease)
        }
    );
    assert_eq!(fast_at, Duration::ZERO, "另一台主机的请求不该等慢主机的 30 秒");
    assert_eq!(fast.endpoint().host, "fast");
    assert_eq!(slow.endpoint().host, "slow");
}

#[tokio::test]
async fn failed_dial_is_not_parked_and_the_next_caller_retries() {
    let h = Harness::new(PoolOptions::default());
    h.factory.fail_times.store(1, Ordering::SeqCst);
    let err = h.pool.acquire(key("db1"), LeaseKind::Shell).await.unwrap_err();
    assert!(matches!(err, dbhx_session::SessionError::Ssh(SshError::AuthFailed)));
    assert_eq!(h.pool.live_count(), 0, "失败的拨号不能留在池里");

    let lease = h.acquire("db1").await;
    assert_eq!(h.dial_count(), 2, "上一次失败不该被缓存成永久失败");
    assert_eq!(lease.endpoint().host, "db1");
}

#[tokio::test(start_paused = true)]
async fn parked_connection_is_dropped_when_ttl_expires() {
    let ttl = Duration::from_secs(60);
    let h = Harness::new(PoolOptions { idle_ttl: ttl, max_idle: 8 });
    let lease = h.acquire("db1").await;
    assert_eq!(h.pool.idle_count(), 0);
    drop(lease);
    assert_eq!(h.pool.idle_count(), 1, "最后一个 lease 释放 ⇒ 进入 idle park");

    settle().await;
    assert_eq!(h.pool.live_count(), 0, "TTL 到点必须真的断开");
    assert_eq!(h.factory.last().close_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn zero_ttl_parks_until_exit() {
    let h = Harness::new(PoolOptions { idle_ttl: Duration::ZERO, max_idle: 8 });
    drop(h.acquire("db1").await);

    settle().await;
    assert_eq!(h.pool.live_count(), 1, "0 表示保留到退出，不是立刻断开");
    assert_eq!(h.factory.last().close_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_active_lease_is_never_evicted_by_ttl() {
    let h = Harness::new(PoolOptions { idle_ttl: Duration::from_secs(60), max_idle: 8 });
    let lease = h.acquire("db1").await;

    settle().await;
    assert_eq!(h.pool.live_count(), 1);
    assert_eq!(h.factory.last().close_count(), 0, "用户正在用的连接不能被超时收走");

    drop(lease);
    settle().await;
    assert_eq!(h.pool.live_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn idle_cap_evicts_the_oldest_parked_connection() {
    let h = Harness::new(PoolOptions { idle_ttl: Duration::from_secs(3600), max_idle: 1 });
    drop(h.acquire("db1").await);
    // 让两条连接的空闲时刻有先后（暂停的时钟里同一时刻无法比新旧）
    tokio::time::sleep(Duration::from_millis(1)).await;
    drop(h.acquire("db2").await);
    tokio::time::sleep(Duration::from_millis(1)).await;

    let third = h.acquire("db3").await;
    assert_eq!(h.dial_count(), 3);
    assert_eq!(h.pool.live_count(), 2, "超出 max_idle 的最早空闲连接被提前淘汰");
    assert_eq!(h.factory.conn(0).close_count(), 1, "淘汰的是 db1");
    assert_eq!(h.factory.conn(1).close_count(), 0, "较新的 db2 留下");
    assert_eq!(third.endpoint().host, "db3");
}

#[tokio::test(start_paused = true)]
async fn remote_close_removes_the_entry_so_the_next_acquire_redials() {
    let h = Harness::new(PoolOptions::default());
    let lease = h.acquire("db1").await;
    h.factory.last().kill_remotely();
    drop(lease);

    settle().await;
    assert_eq!(h.pool.live_count(), 0, "supervisor 应当摘掉死连接");

    let again = h.acquire("db1").await;
    assert_eq!(h.dial_count(), 2);
    assert_eq!(again.state(), SessionState::Authenticated);
}

#[tokio::test(start_paused = true)]
async fn shutdown_closes_everything_including_held_leases() {
    let h = Harness::new(PoolOptions::default());
    let held = h.acquire("db1").await;
    drop(h.acquire("db2").await);
    assert_eq!(h.pool.live_count(), 2);

    h.pool.shutdown().await;
    assert_eq!(h.pool.live_count(), 0);
    let closes: usize = (0..h.factory.dial_count()).map(|i| h.factory.conn(i).close_count()).sum();
    assert_eq!(closes, 2);

    // 持有中的 lease 之后 drop 既不能 panic，也不该把谁重新请回池里
    drop(held);
    assert_eq!(h.pool.idle_count(), 0);
}

#[tokio::test]
async fn a_lease_derefs_to_its_connection() {
    let h = Harness::new(PoolOptions::default());
    let lease = h.acquire("db1").await;
    assert_eq!(lease.endpoint().host, "db1");
    assert_eq!(lease.kind(), LeaseKind::Shell);
    assert_eq!(lease.key(), &key("db1"));
    assert_eq!(lease.state(), SessionState::Authenticated);
    // 假连接的通道一律报错，但至少证明 Deref 指的是同一条连接
    assert!(matches!(
        lease.open_shell("xterm".into(), PtySize::default()).await,
        Err(SshError::Closed)
    ));
}
