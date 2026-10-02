//! 缓存层封装：redis（deadpool-redis）与进程内存（moka，feature = "cache-memory"）
//! 双后端。`Cache` 提取器与读写方法对业务代码完全一致，后端由 `[cache].type`
//! 选择（auto = redis.url 非空走 redis，否则 memory）。

pub mod pool;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRef;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::{CacheConfig, RedisConfig};
use crate::error::AppError;
use crate::state::AppState;

/// 缓存操作错误
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("create redis pool failed: {0}")]
    Create(#[from] deadpool_redis::CreatePoolError),
    #[error("redis pool error: {0}")]
    Pool(#[from] deadpool_redis::PoolError),
    #[error("redis error: {0}")]
    Redis(#[from] deadpool_redis::redis::RedisError),
    #[error("cache serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("operation requires the redis cache backend (cache.type = redis)")]
    RequiresRedis,
    #[error("cache.type = memory requires the \"cache-memory\" feature")]
    MemoryFeatureDisabled,
    #[error("unknown cache.type: {0} (expected auto / redis / memory)")]
    UnknownBackend(String),
}

/// 内存后端条目：值 + 写入时指定的 ttl（供 Expiry 在创建/更新时计算过期）
#[cfg(feature = "cache-memory")]
#[derive(Debug, Clone)]
struct MemEntry {
    value: String,
    ttl: Option<Duration>,
}

/// 按「条目自身 ttl，缺省用配置的默认 ttl」计算过期；两者皆无则不过期
#[cfg(feature = "cache-memory")]
struct MemExpiry {
    default_ttl: Option<Duration>,
}

#[cfg(feature = "cache-memory")]
impl moka::Expiry<String, MemEntry> for MemExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        entry: &MemEntry,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        entry.ttl.or(self.default_ttl)
    }

    fn expire_after_update(
        &self,
        _key: &String,
        entry: &MemEntry,
        _updated_at: std::time::Instant,
        _current_duration: Option<Duration>,
    ) -> Option<Duration> {
        entry.ttl.or(self.default_ttl)
    }
}

/// 缓存封装（redis / 内存双后端）。handler 里声明 `cache: Cache` 即可使用，
/// 方法语义一致；仅 redis 支持的能力（如分布式锁）在 memory 后端报
/// [`CacheError::RequiresRedis`]。
#[derive(Debug, Clone)]
pub struct Cache {
    backend: Backend,
    /// 进程内按 key 串行化回源（防击穿）：key -> 锁，全实例克隆共享
    inflight: std::sync::Arc<InflightMap>,
}

type InflightMap = std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

#[derive(Debug, Clone)]
enum Backend {
    Redis(deadpool_redis::Pool),
    #[cfg(feature = "cache-memory")]
    Memory(moka::future::Cache<String, MemEntry>),
}

impl Cache {
    pub async fn get_string(&self, key: &str) -> Result<Option<String>, CacheError> {
        match &self.backend {
            Backend::Redis(p) => {
                let mut conn = p.get().await?;
                let v = deadpool_redis::redis::cmd("GET")
                    .arg(key)
                    .query_async::<Option<String>>(&mut conn)
                    .await?;
                Ok(v)
            }
            #[cfg(feature = "cache-memory")]
            Backend::Memory(m) => Ok(m.get(key).await.map(|e| e.value)),
        }
    }

    pub async fn set_string(
        &self,
        key: &str,
        value: &str,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        match &self.backend {
            Backend::Redis(p) => {
                let mut conn = p.get().await?;
                let mut cmd = deadpool_redis::redis::cmd("SET");
                cmd.arg(key).arg(value);
                if let Some(d) = ttl {
                    cmd.arg("EX").arg(d.as_secs().max(1));
                }
                cmd.query_async::<()>(&mut conn).await?;
                Ok(())
            }
            #[cfg(feature = "cache-memory")]
            Backend::Memory(m) => {
                m.insert(key.to_string(), MemEntry { value: value.to_string(), ttl })
                    .await;
                Ok(())
            }
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, CacheError> {
        let Some(s) = self.get_string(key).await? else {
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

    pub async fn set_json<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        self.set_string(key, &serde_json::to_string(value)?, ttl)
            .await
    }

    pub async fn del(&self, key: &str) -> Result<(), CacheError> {
        match &self.backend {
            Backend::Redis(p) => {
                let mut conn = p.get().await?;
                deadpool_redis::redis::cmd("DEL")
                    .arg(key)
                    .query_async::<()>(&mut conn)
                    .await?;
                Ok(())
            }
            #[cfg(feature = "cache-memory")]
            Backend::Memory(m) => {
                m.invalidate(key).await;
                Ok(())
            }
        }
    }

    /// 原子计数：key 不存在时从 0 起算（等价 redis `INCRBY`），新建计数器无 TTL。
    /// redis 后端用原生 `INCRBY`（多实例共享同一计数）；memory 后端按 key 串行
    /// 保证原子性（进程内正确，进程间无共享——限流/分布式计数请配 redis 后端）。
    pub async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError> {
        match &self.backend {
            Backend::Redis(p) => {
                let mut conn = p.get().await?;
                let v = deadpool_redis::redis::cmd("INCRBY")
                    .arg(key)
                    .arg(delta)
                    .query_async::<i64>(&mut conn)
                    .await?;
                Ok(v)
            }
            #[cfg(feature = "cache-memory")]
            Backend::Memory(_) => {
                // 进程内按 key 串行（复用 get_or_load 的防击穿锁），读-改-写原子
                let lock = self.inflight_lock(key);
                let _guard = lock.lock().await;
                let current = match self.get_string(key).await? {
                    None => 0,
                    Some(s) => match s.parse::<i64>() {
                        Ok(v) => v,
                        Err(_) => {
                            tracing::warn!(key = %key, value = %s, "cache incr on non-numeric value, reset to 0");
                            0
                        }
                    },
                };
                let next = current + delta;
                self.set_string(key, &next.to_string(), None).await?;
                self.maybe_release_inflight(key, &lock);
                Ok(next)
            }
        }
    }

    /// 重设 key 的 TTL：`ttl` 传 `None`/`Duration::ZERO` 表示永不过期（等价
    /// redis `PERSIST`）；key 不存在返回 `Ok(false)`。
    /// memory 后端实现为「读出 → 带 TTL 重写」，并发下的单 key 过期重设窗口
    /// 极小；对原子语义敏感的场景（计数器 + 过期联动）请用 redis 后端。
    pub async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        match &self.backend {
            Backend::Redis(p) => {
                let mut conn = p.get().await?;
                let touched: i64 = match ttl {
                    Some(d) if d > Duration::ZERO => {
                        deadpool_redis::redis::cmd("EXPIRE")
                            .arg(key)
                            .arg(d.as_secs().max(1))
                            .query_async::<i64>(&mut conn)
                            .await?
                    }
                    _ => {
                        deadpool_redis::redis::cmd("PERSIST")
                            .arg(key)
                            .query_async::<i64>(&mut conn)
                            .await?
                    }
                };
                Ok(touched == 1)
            }
            #[cfg(feature = "cache-memory")]
            Backend::Memory(_) => {
                let lock = self.inflight_lock(key);
                let _guard = lock.lock().await;
                let Some(value) = self.get_string(key).await? else {
                    self.maybe_release_inflight(key, &lock);
                    return Ok(false);
                };
                // moka 条目 TTL 写入时固定：重写条目携带新 TTL
                self.set_string(key, &value, ttl.filter(|d| *d > Duration::ZERO))
                    .await?;
                self.maybe_release_inflight(key, &lock);
                Ok(true)
            }
        }
    }

    pub(crate) async fn ping(&self) -> Result<(), CacheError> {
        match &self.backend {
            Backend::Redis(p) => {
                let mut conn = p.get().await?;
                deadpool_redis::redis::cmd("PING").query_async::<()>(&mut conn).await?;
                Ok(())
            }
            #[cfg(feature = "cache-memory")]
            Backend::Memory(_) => Ok(()), // 进程内缓存无独立组件，进程在即可用
        }
    }

    /// redis 后端时返回 true（分布式锁等 redis 专属能力依赖此后端）
    pub fn is_redis(&self) -> bool {
        self.redis_pool().is_some()
    }

    /// redis 后端时返回底层连接池（分布式锁等 redis 专属能力使用）
    pub(crate) fn redis_pool(&self) -> Option<&deadpool_redis::Pool> {
        match &self.backend {
            Backend::Redis(p) => Some(p),
            #[cfg(feature = "cache-memory")]
            Backend::Memory(_) => None,
        }
    }

    /// cache-aside 一站式读取：先查缓存，miss（**或缓存故障**）时用 `load` 回源并回填。
    ///
    /// - 缓存读写失败只记日志、按 miss 处理，**不会传染成业务 500**（redis 抖动时接口自动退化为直连 DB）；
    /// - 回源前按 key 进程内串行（防击穿）：同一 key 并发 miss 时只有一个请求真正回源，
    ///   其余等待后二次读缓存；跨实例的击穿防护可在 `load` 闭包里配合 `dist-lock` 实现；
    /// - `load` 的错误类型 `E` 原样透传（通常是 `AppError`）。
    ///
    /// ```no_run
    /// # use core_rs::prelude::*;
    /// # use std::time::Duration;
    /// # async fn demo(cache: Cache, db: Db) -> AppResult<()> {
    /// let user: User = cache
    ///     .get_or_load("user:42", Some(Duration::from_secs(300)), || async {
    ///         load_user(&db, 42).await
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// #
    /// # #[derive(serde::Serialize, serde::Deserialize)]
    /// # struct User;
    /// # async fn load_user(db: &Db, id: u64) -> Result<User, AppError> { Ok(User) }
    /// ```
    pub async fn get_or_load<T, E, F, Fut>(
        &self,
        key: &str,
        ttl: Option<Duration>,
        load: F,
    ) -> Result<T, E>
    where
        T: Serialize + DeserializeOwned + Send,
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

        // 进程内按 key 串行（防击穿），拿到锁后二次读缓存
        let lock = self.inflight_lock(key);
        let value = {
            let _guard = lock.lock().await;
            if let Ok(Some(v)) = self.get_json::<T>(key).await {
                return Ok(v);
            }
            // 回源；回填失败只记日志
            let value = load().await?;
            if let Err(e) = self.set_json(key, &value, ttl).await {
                tracing::warn!(key = %key, error = %e, "cache write failed after load");
            }
            value
        };
        self.maybe_release_inflight(key, &lock);
        Ok(value)
    }

    fn inflight_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        Self::lock_map(&self.inflight)
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    /// 锁不再有等待者时清出 map，避免 key 无限累积
    fn maybe_release_inflight(&self, key: &str, lock: &Arc<tokio::sync::Mutex<()>>) {
        let mut map = Self::lock_map(&self.inflight);
        if let Some(existing) = map.get(key) {
            // 仅 map 与当前持有者两个引用时（无并发等待者）才移除；
            // Arc 比对防止把后来者新插入的同名锁误删
            if Arc::ptr_eq(existing, lock) && Arc::strong_count(existing) <= 2 {
                map.remove(key);
            }
        }
    }

    /// 毒化恢复而非 panic：临界区内不执行任何业务代码，毒化只可能源于框架自身
    /// bug；遵循「运行期框架只返回/降级，不 panic」直接取回数据继续工作
    fn lock_map(
        m: &InflightMap,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>
    {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 按 `[cache]` + `[redis]` 配置构建缓存实例（Application 装配时自动调用，
/// 应用想脱离 Application 手动装配时也可直接使用）。
/// 返回 `None` 表示未启用缓存（redis url 为空且 memory 不可用/未启用）。
pub fn build(
    cache_cfg: &CacheConfig,
    redis_cfg: &RedisConfig,
) -> Result<Option<Cache>, CacheError> {
    match cache_cfg.backend.as_str() {
        "redis" => {
            // 与既有约定一致：url 为空即禁用，提取器在使用时报错
            if redis_cfg.url.is_empty() {
                return Ok(None);
            }
            Ok(Some(pool::connect(redis_cfg)?))
        }
        "memory" => memory_backend(cache_cfg, true),
        "auto" => {
            if !redis_cfg.url.is_empty() {
                Ok(Some(Cache {
                    backend: Backend::Redis(pool::connect_pool(redis_cfg)?),
                    inflight: Default::default(),
                }))
            } else {
                memory_backend(cache_cfg, false)
            }
        }
        other => Err(CacheError::UnknownBackend(other.to_string())),
    }
}

#[cfg(feature = "cache-memory")]
fn memory_backend(cfg: &CacheConfig, _explicit: bool) -> Result<Option<Cache>, CacheError> {
    let default_ttl = (cfg.memory.default_ttl_secs > 0)
        .then(|| Duration::from_secs(cfg.memory.default_ttl_secs));
    let cache = moka::future::Cache::builder()
        .max_capacity(cfg.memory.max_capacity)
        .expire_after(MemExpiry { default_ttl })
        .build();
    Ok(Some(Cache {
        backend: Backend::Memory(cache),
        inflight: Default::default(),
    }))
}

#[cfg(not(feature = "cache-memory"))]
fn memory_backend(_cfg: &CacheConfig, explicit: bool) -> Result<Option<Cache>, CacheError> {
    if explicit {
        return Err(CacheError::MemoryFeatureDisabled);
    }
    // auto 且未启用 cache-memory feature：降级为无缓存，保持零配置可跑
    tracing::warn!("cache.type=auto fell back to memory but \"cache-memory\" feature is not enabled; cache disabled");
    Ok(None)
}

impl axum::extract::FromRef<AppState> for Option<Cache> {
    fn from_ref(input: &AppState) -> Self {
        input.cache.clone()
    }
}

impl<S> axum::extract::FromRequestParts<S> for Cache
where
    S: Send + Sync,
    AppState: axum::extract::FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        AppState::from_ref(state)
            .cache
            .clone()
            .ok_or_else(|| AppError::internal("cache not configured"))
    }
}
