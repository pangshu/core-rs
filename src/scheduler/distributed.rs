//! 分布式选主（基于 `cache` 的分布式锁）。
//!
//! 多实例部署时，同一任务在每个实例都会被调度；执行前抢一把
//! `scheduler:{job}` 的锁，**抢到才执行**——即「每次触发做一次选主」，
//! 简单且无需额外协调服务（见 `docs/02` §12）。
//!
//! ⚠️ 启动期组合校验要求：`[scheduler].distributed.enabled = true` 时，
//! `cache` 必须是 `redis` 后端（进程内锁无法跨实例共享），否则 **fail-fast**。

use std::time::Duration;

use crate::cache::{CacheService, DistributedLock};
use crate::errors::AppResult;

/// 选主器。
#[derive(Debug, Clone)]
pub struct LeaderElection {
    cache: CacheService,
    lock_ttl: Duration,
}

impl LeaderElection {
    /// 以缓存与锁 TTL 创建。
    #[must_use]
    pub fn new(cache: CacheService, lock_ttl: Duration) -> Self {
        Self { cache, lock_ttl }
    }

    /// 尝试成为本次触发的执行者。
    ///
    /// 返回 `Ok(Some(guard))` 表示抢到（**drop 即释放**）；`Ok(None)` 表示其他实例在跑。
    ///
    /// # Errors
    /// 缓存不可用时返回 `50004`。
    pub async fn try_acquire(&self, job: &str) -> AppResult<Option<DistributedLock>> {
        self.cache.lock(&lock_key(job), Some(self.lock_ttl)).await
    }
}

/// 选主锁键（`cache` 会再加 `{ns}:l:` 前缀）。
fn lock_key(job: &str) -> String {
    format!("sched:{job}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::cache::MemoryCache;
    use crate::config::CacheSection;

    use super::*;

    fn service() -> CacheService {
        CacheService::new(Arc::new(MemoryCache::default()), CacheSection::default())
    }

    #[tokio::test]
    async fn should_allow_only_one_leader_per_job() {
        let election = LeaderElection::new(service(), Duration::from_secs(30));

        let first = election.try_acquire("daily-cleanup").await.unwrap();
        assert!(first.is_some(), "首个实例应抢到");

        let second = election.try_acquire("daily-cleanup").await.unwrap();
        assert!(second.is_none(), "同任务不应被并发执行");

        let other = election.try_acquire("other-job").await.unwrap();
        assert!(other.is_some(), "不同任务互不影响");

        drop(first);
        let again = election.try_acquire("daily-cleanup").await.unwrap();
        assert!(again.is_some(), "释放后应可再次抢到");
    }

    #[test]
    fn should_prefix_lock_key() {
        assert_eq!(lock_key("cleanup"), "sched:cleanup");
    }
}
