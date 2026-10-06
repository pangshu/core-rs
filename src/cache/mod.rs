//! 缓存与锁（可插拔：内存 / Redis，配置选择，文档 三·11）。
//!
//! - [`Cache`] / [`Lock`] trait 是唯一契约，业务只依赖 trait，**换后端不改一行业务代码**；
//! - [`memory`]：moka 进程内缓存 + 进程内锁（feature = "cache-memory"，默认），
//!   单机部署与开发/测试的默认值；
//! - [`redis`]：deadpool-redis 连接池 + `SET NX PX` 分布式锁（feature = "cache-redis"），
//!   多实例部署使用；限流 / 幂等 / 分布式锁等需要**跨进程一致**的场景必须 redis；
//! - [`lock`]：`Lock` trait + [`LockGuard`]（释放 Lua 校验持有者 / Drop 兜底），
//!   是限流、防重、幂等的公共底座。

#[cfg(feature = "cache-memory")]
pub mod memory;
#[cfg(feature = "cache-redis")]
pub mod redis;
pub mod lock;

pub use lock::{Lock, LockError, LockGuard, LockHandle, MemoryLock, build_lock};

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::sections::CacheSettings;

/// 缓存操作错误
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("create redis pool failed: {0}")]
    #[cfg(feature = "cache-redis")]
    Create(#[from] deadpool_redis::CreatePoolError),
    #[error("redis pool error: {0}")]
    #[cfg(feature = "cache-redis")]
    Pool(#[from] deadpool_redis::PoolError),
    #[error("redis error: {0}")]
    #[cfg(feature = "cache-redis")]
    Redis(#[from] deadpool_redis::redis::RedisError),
    #[error("cache serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("cache backend `{0}` requires a feature (cache-memory / cache-redis) that is not enabled")]
    FeatureDisabled(String),
    #[error("cache backend `{0}` is misconfigured: {1}")]
    Config(String, String),
    #[error("unknown cache backend: {0} (expected memory / redis)")]
    UnknownBackend(String),
}

/// 缓存契约（string 级原语，保持对象安全；JSON / 回源便捷方法见 [`CacheExt`]）。
#[async_trait::async_trait]
pub trait Cache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>;
    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError>;
    async fn del(&self, key: &str) -> Result<(), CacheError>;
    /// 原子计数：key 不存在时从 0 起算（等价 redis `INCRBY`），新建计数器无 TTL。
    /// memory 后端进程内正确，进程间无共享——限流/分布式计数请配 redis 后端。
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError>;
    /// 重设 key 的 TTL：`None`/`Duration::ZERO` 表示永不过期（等价 `PERSIST`）；
    /// key 不存在返回 `Ok(false)`。
    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError>;
    /// 存活探测（/ready 用）。memory 后端恒 Ok。
    async fn ping(&self) -> Result<(), CacheError>;
}

/// 便捷方法（JSON 读写 / cache-aside 一站式回源），对 `dyn Cache` 同样可用。
#[allow(async_fn_in_trait)]
pub trait CacheExt: Cache {
    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, CacheError> {
        let Some(s) = self.get(key).await? else {
            return Ok(None);
        };
        match serde_json::from_str(&s) {
            Ok(v) => Ok(Some(v)),
            // 脏数据（如缓存结构变更）按 miss 处理让业务回源，而不是打挂接口
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "cache value corrupt, treat as miss");
                Ok(None)
            }
        }
    }

    async fn set_json<T: Serialize + Sync>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        self.set(key, &serde_json::to_string(value)?, ttl).await
    }

    /// cache-aside 一站式读取：先查缓存，miss（**或缓存故障**）时用 `load` 回源并回填。
    /// 缓存读写失败只记日志、按 miss 处理，**不会传染成业务 500**；
    /// `load` 的错误类型 `E` 原样透传（通常是 `AppError`）。
    async fn get_or_load<T, E, F, Fut>(&self, key: &str, ttl: Option<Duration>, load: F) -> Result<T, E>
    where
        T: Serialize + DeserializeOwned + Send + Sync,
        E: From<CacheError>,
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, E>> + Send,
    {
        // 读缓存：任何缓存故障按 miss 降级，不传染
        match self.get_json::<T>(key).await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "cache read failed, degrade to direct load")
            }
        }
        let value = load().await?;
        if let Err(e) = self.set_json(key, &value, ttl).await {
            tracing::warn!(key = %key, error = %e, "cache write failed after load");
        }
        Ok(value)
    }
}

impl<T: Cache + ?Sized> CacheExt for T {}

/// 共享缓存句柄（存于 CoreState）
pub type CacheHandle = std::sync::Arc<dyn Cache>;

/// 按 `[cache]` 配置构建缓存实例（App 装配时自动调用；手动装配亦可用）。
pub fn build_cache(settings: &CacheSettings) -> Result<CacheHandle, CacheError> {
    match settings.backend.as_str() {
        "memory" => {
            #[cfg(feature = "cache-memory")]
            {
                Ok(std::sync::Arc::new(memory::MemoryCache::new(&settings.memory)))
            }
            #[cfg(not(feature = "cache-memory"))]
            {
                Err(CacheError::FeatureDisabled("memory".to_string()))
            }
        }
        "redis" => {
            #[cfg(feature = "cache-redis")]
            {
                if !settings.redis.enabled() {
                    return Err(CacheError::Config(
                        "redis".to_string(),
                        "cache.backend = redis but cache.redis.url is empty".to_string(),
                    ));
                }
                Ok(std::sync::Arc::new(redis::RedisCache::new(&settings.redis)?))
            }
            #[cfg(not(feature = "cache-redis"))]
            {
                Err(CacheError::FeatureDisabled("redis".to_string()))
            }
        }
        other => Err(CacheError::UnknownBackend(other.to_string())),
    }
}
