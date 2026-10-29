//! 限流状态存储。
//!
//! 当前实现为**进程内**（GCRA / `governor`）；多实例需共享 store
//! （`[ratelimit].store = "redis"`，见 `docs/02` §12）。
//!
//! ## 键空间必须有人管
//!
//! `governor` 的 keyed 限流器**不会自己回收键**：`retain_recent()` 是需要调用方
//! 主动触发的接口。不调用时，一个键只被访问一次也会永久留在 `DashMap` 里
//! （每键约 100~200 字节）。而限流键的来源可以完全由攻击者控制——只要他能伪造
//! `X-Forwarded-For`（`trusted_proxies` 过宽、或链路里没有 `ConnectInfo`），
//! 逐请求换一个 IP 就能每秒造出上万个新键，几百万键即数百 MB，进程直接 OOM
//! 被杀。**这不是「限流不准」，是整站挂掉。**
//!
//! 因此 [`MemoryStore`] 自带两层保护：
//!
//! 1. **摊销回收**：每 `max_keys / 8`（夹在 `[16, 1024]`）次检查做一次维护，
//!    键数过半阈值时调 `retain_recent()`，丢掉「与空桶不可区分」的键（GCRA
//!    语义下等价于不存在）。稳态键数 ≈ 一个窗口内出现过的不同来源数。
//! 2. **硬上限**：回收后仍超过 `max_keys` 时进入 `saturated` 状态，新流量统一
//!    落到一个全局兜底桶（配额仍是 `limit`/`window`）。键洪泛攻击下这会退化成
//!    「全局限流」——比 OOM 好，且攻击结束后下一次维护即自动恢复。

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use governor::clock::{Clock, DefaultClock};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};

/// 键数上限的默认值（`[ratelimit].max_keys`）。
pub const DEFAULT_MAX_KEYS: usize = 100_000;

/// 每多少次检查做一次维护（回收 + 上限判定）的上限值。
///
/// 维护里含 `len()`（DashMap 需遍历分片计数），逐请求做会把限流变成 O(n)；
/// 按 `max_keys / 8` 摊销（夹在 `[16, 1024]`），单次成本可忽略，且「超限后
/// 还会多插入多少个键」被压到 `max_keys / 8` 以内。
const MAINTAIN_EVERY_MAX: u64 = 1024;

/// 维护频率的下限（避免小 `max_keys` 时每次请求都维护）。
const MAINTAIN_EVERY_MIN: u64 = 16;

/// 键数超限后的全局兜底桶。
const OVERFLOW_KEY: &str = "ip:overflow";

/// 限流状态存储（object-safe）。
///
/// `check` 是 async 的：`memory` 实现是纯计算（同步完成），
/// `redis` 实现要发一次 Lua 脚本——统一成 async 才能共用同一套中间件。
#[async_trait]
pub trait RateLimitStore: Send + Sync + 'static {
    /// 计数窗口。
    fn window(&self) -> Duration;

    /// 检查并消耗一个额度；`Err(wait)` 为建议等待时长。
    async fn check(&self, key: &str) -> Result<(), Duration>;
}

#[async_trait]
impl RateLimitStore for MemoryStore {
    fn window(&self) -> Duration {
        self.window
    }

    async fn check(&self, key: &str) -> Result<(), Duration> {
        // 纯内存计算，不需要真正的异步
        MemoryStore::check_now(self, key)
    }
}

/// 进程内限流存储（按 key 独立计数）。
pub struct MemoryStore {
    limiter: DefaultKeyedRateLimiter<String>,
    window: Duration,
    max_keys: usize,
    maintain_every: u64,
    ops: AtomicU64,
    saturated: AtomicBool,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("window", &self.window)
            .field("max_keys", &self.max_keys)
            .field("keys", &self.limiter.len())
            .field("saturated", &self.saturated.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl MemoryStore {
    /// 以「`limit` 次 / `window`」创建（GCRA，允许 `limit` 的突发额度）。
    #[must_use]
    pub fn new(limit: u32, window: Duration) -> Self {
        Self::with_max_keys(limit, window, DEFAULT_MAX_KEYS)
    }

    /// 同 [`new`](Self::new)，但显式指定键数上限（`[ratelimit].max_keys`）。
    #[must_use]
    pub fn with_max_keys(limit: u32, window: Duration, max_keys: usize) -> Self {
        let burst = NonZeroU32::new(limit.max(1)).unwrap_or(NonZeroU32::MIN);
        let window = if window.is_zero() {
            Duration::from_secs(1)
        } else {
            window
        };
        // `Quota::new(max_burst, replenish_all_per)` 语义正是「window 内 limit 次、
        // 允许 limit 的突发额度」。governor 0.8.1 把它标为 deprecated 并建议
        // 「`with_period` + `max_burst`」，但该版本并没有 `max_burst` 方法，
        // `with_period` 只能表达 burst=1，故这里保留 `Quota::new`。
        #[allow(deprecated)]
        let quota = Quota::new(burst, window).unwrap_or_else(|| Quota::per_second(burst));
        let max_keys = max_keys.max(1);
        Self {
            limiter: RateLimiter::keyed(quota),
            window,
            max_keys,
            maintain_every: (max_keys / 8).clamp(
                usize::try_from(MAINTAIN_EVERY_MIN).unwrap_or(16),
                usize::try_from(MAINTAIN_EVERY_MAX).unwrap_or(1024),
            ) as u64,
            ops: AtomicU64::new(0),
            saturated: AtomicBool::new(false),
        }
    }

    /// 计数窗口。
    #[must_use]
    pub fn window(&self) -> Duration {
        self.window
    }

    /// 当前驻留的键数量（排障 / 测试用；`DashMap` 下为遍历计数）。
    #[must_use]
    pub fn tracked_keys(&self) -> usize {
        self.limiter.len()
    }

    /// 是否因键数超限而降级到全局兜底桶。
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.saturated.load(Ordering::Relaxed)
    }

    /// 检查并消耗一个额度。
    ///
    /// `Ok(())` 放行；`Err(wait)` 表示需等待多久（用于 `Retry-After`）。
    pub fn check_now(&self, key: &str) -> Result<(), Duration> {
        self.maintain_if_due();

        // 键数超限时不再为**新**来源建桶，统一落到全局兜底桶。
        // 注意：这里不区分「新键 / 已有键」（`governor` 没有暴露 contains），
        // 超限期间所有流量共用兜底桶——这正是攻击下想要的「应急刹车」，
        // 攻击结束后下一次维护即恢复按来源计数。
        let key = if self.saturated.load(Ordering::Relaxed) {
            OVERFLOW_KEY
        } else {
            key
        };

        match self.limiter.check_key(&key.to_string()) {
            Ok(_) => Ok(()),
            Err(not_until) => {
                let now = DefaultClock::default().now();
                Err(not_until.wait_time_from(now))
            }
        }
    }

    /// 每 `maintain_every` 次检查做一次回收 + 上限判定。
    fn maintain_if_due(&self) {
        let ops = self.ops.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if !ops.is_multiple_of(self.maintain_every) {
            return;
        }

        // 先回收：半阈值就开始，避免正好撞上限才动手
        if self.limiter.len() > self.max_keys / 2 {
            self.limiter.retain_recent();
            self.limiter.shrink_to_fit();
        }

        let over = self.limiter.len() > self.max_keys;
        if over {
            if !self.saturated.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    max_keys = self.max_keys,
                    "限流键数量超过上限（疑似来源伪造或大规模扫描），新流量降级到全局兜底桶"
                );
            }
        } else {
            self.saturated.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_allow_up_to_limit_within_window() {
        let store = MemoryStore::new(3, Duration::from_secs(60));
        for _ in 0..3 {
            assert!(store.check_now("a").is_ok());
        }
        assert!(store.check_now("a").is_err(), "第 4 次应被限流");
    }

    #[tokio::test]
    async fn should_enforce_limit_through_trait() {
        // 中间件走的是 trait 的 async `check`，这里覆盖该路径
        let store = MemoryStore::new(1, Duration::from_secs(60));
        let boxed: std::sync::Arc<dyn RateLimitStore> = std::sync::Arc::new(store);
        assert!(boxed.check("a").await.is_ok());
        assert!(boxed.check("a").await.is_err());
    }

    #[test]
    fn should_isolate_keys() {
        let store = MemoryStore::new(1, Duration::from_secs(60));
        assert!(store.check_now("a").is_ok());
        assert!(store.check_now("a").is_err());
        assert!(store.check_now("b").is_ok(), "不同 key 独立计数");
    }

    #[test]
    fn should_report_positive_wait_time() {
        let store = MemoryStore::new(1, Duration::from_secs(60));
        assert!(store.check_now("a").is_ok());
        let wait = store.check_now("a").unwrap_err();
        assert!(wait > Duration::ZERO, "应给出正数等待时长");
        assert!(wait <= Duration::from_secs(60));
    }

    #[test]
    fn should_tolerate_zero_limit_and_window() {
        let store = MemoryStore::new(0, Duration::ZERO);
        assert!(store.check_now("a").is_ok(), "limit=0 收敛为 1");
        assert_eq!(store.window(), Duration::from_secs(1));
    }

    /// 回归：键数量必须有上限，不能被「每次换一个键」的流量顶爆内存。
    ///
    /// 修复前 `governor` 的 `retain_recent()` 从未被调用，键只增不减：
    /// 攻击者伪造 XFF 逐请求换 IP 即可把进程打到 OOM。
    #[test]
    fn should_cap_key_growth_and_degrade_to_overflow_bucket() {
        // 上限 64 个键：制造 10 万个不同键
        let store = MemoryStore::with_max_keys(1000, Duration::from_secs(60), 64);
        for i in 0..100_000u32 {
            // 配额足够大，这里只关心键空间
            let _ = store.check_now(&format!(
                "ip:10.{}.{}.{}",
                i >> 16,
                (i >> 8) & 0xff,
                i & 0xff
            ));
        }

        let tracked = store.tracked_keys();
        // 超限检测是摊销的，最多多插 `max_keys / 8` 个键
        assert!(
            tracked <= 64 + 64 / 8,
            "键数应被压在上限附近，实际 {tracked} 个"
        );
        assert!(store.is_saturated(), "超限后应进入兜底状态");
    }

    /// 回收必须真的生效：窗口内的键被保留，窗口外（已补满）的键被丢弃。
    #[test]
    fn should_reclaim_stale_keys() {
        // 窗口极短：桶很快补满，键随即「与空桶不可区分」。
        // `max_keys` 取小值，让「过半阈值才回收」的条件真的被触发。
        let store = MemoryStore::with_max_keys(10, Duration::from_millis(1), 8);
        for i in 0..10u32 {
            let _ = store.check_now(&format!("k{i}"));
        }
        std::thread::sleep(Duration::from_millis(20));
        // 触发一次维护（1024 是任何 maintain_every 的整数倍）
        for _ in 0..1024 {
            let _ = store.check_now("warm");
        }
        assert_eq!(store.tracked_keys(), 1, "过期键应被回收，只剩本轮用到的键");
    }
}
