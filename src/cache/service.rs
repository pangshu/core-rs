//! 缓存服务：命名空间、默认 TTL、singleflight、TTL 抖动、空值缓存。
//!
//! 本类型是 **T1（透明加速）**：可丢、可重建、后端异常时降级直读。语义承载类
//! 数据（幂等 / 去重 / 黑名单 / 验证码）请走 [`SecureStore`]（[`CacheService::secure`]）。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

#[cfg(feature = "cache-redis")]
use crate::cache::RedisCache;
use crate::cache::key::{self, Class};
use crate::cache::lock;
use crate::cache::store::SecureStore;
use crate::cache::{Cache, DistributedLock, MemoryCache};
use crate::config::CacheSection;
use crate::config::sections::cache::CacheBackend;
use crate::errors::{AppError, AppResult, ErrorCode};

/// 值信封：`[TAG_VALUE][payload]` 表示「有值」。
///
/// 用值内 tag 而不是带内哨兵字节串，避免用户值恰好等于哨兵时被误判为「不存在」。
const TAG_VALUE: u8 = 1;
/// 值信封：`[TAG_NULL]` 表示「确认不存在」（防穿透的空值缓存）。
const TAG_NULL: u8 = 0;

/// 缓存服务（`AppState.cache`）。
///
/// 以 `Arc` 共享；handler 经 `State<AppState>`（或 `State<CacheService>`，需
/// `FromRef`）访问。
#[derive(Clone)]
pub struct CacheService {
    backend: Arc<dyn Cache>,
    /// Redis 后端实例（`cache-redis` 且 `backend = redis` 时存在），
    /// 供分布式锁路径直接使用——`Cache` trait 不要求 `Any`，无法下转。
    #[cfg(feature = "cache-redis")]
    redis: Option<Arc<RedisCache>>,
    config: CacheSection,
    /// singleflight 闸门：同一 key 的并发回源合并为一次。
    ///
    /// 用 moka 承载以自动回收（容量 + 空闲过期），避免键空间无限增长。
    inflight: moka::sync::Cache<String, Arc<Inflight>>,
    /// 进程内锁注册表（内存后端 / 单实例语义）。
    locks: lock::LockRegistry,
}

/// 一次并发回源的共享状态。
///
/// `gate` 负责串行化，`result` 让等待者**复用**领先者的结果（含 `Ok(None)`）——
/// 否则等待者会逐个重跑 loader，把「防击穿」变成「串行回源」。
struct Inflight {
    gate: AsyncMutex<()>,
    result: OnceLock<Result<Option<Vec<u8>>, SharedError>>,
}

impl Inflight {
    fn new() -> Self {
        Self {
            gate: AsyncMutex::new(()),
            result: OnceLock::new(),
        }
    }
}

/// 可跨等待者共享的错误（`AppError` 不是 `Clone`）。
#[derive(Clone, Debug)]
struct SharedError {
    code: ErrorCode,
    message: String,
}

impl SharedError {
    fn capture(error: &AppError) -> Self {
        match error {
            AppError::Biz { code, message, .. } => Self {
                code: *code,
                message: message.clone(),
            },
            AppError::Internal(_) => Self {
                code: ErrorCode::INTERNAL,
                message: "内部错误".to_string(),
            },
        }
    }

    fn to_app_error(&self) -> AppError {
        AppError::biz(self.code, self.message.clone())
    }
}

impl std::fmt::Debug for CacheService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheService")
            .field("backend", &self.config.backend)
            .field("namespace", &self.config.namespace)
            .finish()
    }
}

impl CacheService {
    /// 由后端与配置装配（内存锁语义；Redis 场景请用 [`from_config`](Self::from_config)）。
    ///
    /// 注意：即使 `config.backend = redis`，本构造函数也**不会**持有 Redis 句柄，
    /// 此时 [`lock`](Self::lock) 会返回 `50005` 而不是静默退化为进程内锁。
    #[must_use]
    pub fn new(backend: Arc<dyn Cache>, config: CacheSection) -> Self {
        Self {
            backend,
            #[cfg(feature = "cache-redis")]
            redis: None,
            config,
            inflight: moka::sync::Cache::builder()
                .max_capacity(10_000)
                // 仅作兜底：领先者 panic 时不会主动 invalidate，靠空闲回收
                .time_to_idle(Duration::from_secs(600))
                .build(),
            locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// 按 `[cache]` 配置装配（`backend = redis` 时建立连接）。
    ///
    /// # Errors
    /// - `backend = redis` 但未启用 `cache-redis` feature → `50005`
    /// - `backend = redis` 但缺少 `redis.url` → `50005`
    /// - Redis 连接失败 → `50004`
    pub async fn from_config(config: &CacheSection) -> AppResult<Self> {
        match config.backend {
            CacheBackend::Memory => Ok(Self::new(
                Arc::new(MemoryCache::new(config.limits.max_memory_bytes)),
                config.clone(),
            )),
            CacheBackend::Redis => {
                #[cfg(feature = "cache-redis")]
                {
                    let url = config.redis.url.as_deref().ok_or_else(|| {
                        AppError::biz(
                            ErrorCode::SERVICE_UNAVAILABLE,
                            "[cache].backend = \"redis\" 但 [cache].redis.url 为空",
                        )
                    })?;
                    let backend = Arc::new(RedisCache::connect(url, config.redis.pool_size).await?);
                    let mut service = Self::new(backend.clone(), config.clone());
                    service.redis = Some(backend);
                    Ok(service)
                }
                #[cfg(not(feature = "cache-redis"))]
                {
                    // 启动期组合校验（docs/02 §12.1）应在此之前拦下；这里兜底。
                    Err(AppError::biz(
                        ErrorCode::SERVICE_UNAVAILABLE,
                        "[cache].backend = \"redis\" 但未启用 cache-redis feature",
                    ))
                }
            }
        }
    }

    /// 当前配置。
    #[must_use]
    pub fn config(&self) -> &CacheSection {
        &self.config
    }

    /// 取得某个语义分类的 T2 存储门面。
    #[must_use]
    pub fn secure(&self, class: Class) -> SecureStore {
        SecureStore::new(self.clone(), class)
    }

    /// 后端引用（供 [`SecureStore`] 使用）。
    pub(crate) fn backend(&self) -> &Arc<dyn Cache> {
        &self.backend
    }

    /// 按分类构造完整键。
    pub(crate) fn class_key(&self, class: Class, rest: &str) -> AppResult<String> {
        key::build(
            &self.config.namespace,
            class,
            rest,
            self.config.limits.max_key_bytes,
        )
    }

    /// 校验值大小上限。
    pub(crate) fn check_value_size(&self, len: usize) -> AppResult<()> {
        if len > self.config.limits.max_value_bytes {
            return Err(AppError::biz(
                ErrorCode::PAYLOAD_TOO_LARGE,
                format!(
                    "缓存值 {} 字节超过上限 {} 字节",
                    len, self.config.limits.max_value_bytes
                ),
            ));
        }
        Ok(())
    }

    /// 读取（自动加命名空间与 `c:` 分类段）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端访问失败时返回 `50004`。
    pub async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        let namespaced = self.class_key(Class::Value, key)?;
        Ok(self.backend.get(&namespaced).await?.and_then(decode))
    }

    /// 写入（自动加命名空间；`ttl = None` 时用 `[cache].default_ttl`）。
    ///
    /// # Errors
    /// 键非法 / TTL 为零返回 `10001`；值超限返回 `10002`；后端访问失败返回 `50004`。
    pub async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> AppResult<()> {
        let namespaced = self.class_key(Class::Value, key)?;
        let ttl = self.effective_ttl(ttl)?;
        self.check_value_size(value.len())?;
        self.backend
            .set(&namespaced, &encode_value(value), Some(ttl))
            .await
    }

    /// 删除（自动加命名空间）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端访问失败时返回 `50004`。
    pub async fn del(&self, key: &str) -> AppResult<()> {
        let namespaced = self.class_key(Class::Value, key)?;
        self.backend.del(&namespaced).await
    }

    /// 是否存在（空值缓存视为不存在）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端访问失败时返回 `50004`。
    pub async fn exists(&self, key: &str) -> AppResult<bool> {
        Ok(self.get(key).await?.is_some())
    }

    /// 连通性探测。
    ///
    /// # Errors
    /// 后端不可用时返回 `50004`。
    pub async fn ping(&self) -> AppResult<()> {
        self.backend.ping().await
    }

    /// 读缓存，未命中则回源并回填（**内置 singleflight / 抖动 / 空值缓存**）。
    ///
    /// 回源失败会向上传播；**缓存读写失败不阻断请求**——只记 warn 并直读回源
    /// （降级，见 `docs/03` §4.11）。
    ///
    /// 并发语义：等待者会复用领先者的结果（含「确认不存在」）；`Err` 默认**不共享**
    /// （避免一次瞬时故障放大成整批失败），可用 `[cache].singleflight.share_errors` 打开。
    ///
    /// 返回 `Ok(None)` 表示「确认不存在」（可能来自空值缓存）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；仅回源失败时返回错误。
    pub async fn get_or_set<F, Fut>(
        &self,
        key: &str,
        ttl: Option<Duration>,
        loader: F,
    ) -> AppResult<Option<Vec<u8>>>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = AppResult<Option<Vec<u8>>>> + Send,
    {
        let namespaced = self.class_key(Class::Value, key)?;

        match self.backend.get(&namespaced).await {
            Ok(Some(bytes)) => return Ok(decode(bytes)),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, key = %namespaced, "缓存读取失败，降级回源");
            }
        }

        if !self.config.singleflight.enabled {
            return self.load_and_fill(&namespaced, ttl, loader).await;
        }

        // 防击穿：同一 key 的并发回源合并为一次（`or_insert_with` 原子建闸）
        let entry = self.inflight_entry(&namespaced);
        let _gate = entry.gate.lock().await;

        // 等闸门期间，领先者可能已完成（或已回填缓存）
        if let Some(shared) = entry.result.get() {
            return shared.clone().map_err(|error| error.to_app_error());
        }
        if let Ok(Some(bytes)) = self.backend.get(&namespaced).await {
            let value = decode(bytes);
            let _ = entry.result.set(Ok(value.clone()));
            self.inflight.invalidate(&namespaced);
            return Ok(value);
        }

        let loaded = match loader().await {
            Ok(value) => value,
            Err(error) => {
                if self.config.singleflight.share_errors {
                    let _ = entry.result.set(Err(SharedError::capture(&error)));
                }
                return Err(error);
            }
        };

        self.fill(&namespaced, &loaded, ttl).await;
        let _ = entry.result.set(Ok(loaded.clone()));
        self.inflight.invalidate(&namespaced);
        Ok(loaded)
    }

    /// 获取分布式锁；已被占用返回 `Ok(None)`。
    ///
    /// `ttl = None` 时用 `[cache].lock.default_ttl`。guard **drop 即释放**。
    ///
    /// # Errors
    /// 键非法 / TTL 为零返回 `10001`；`backend = redis` 但缺少 Redis 连接返回
    /// `50005`（**不**退化为进程内锁）；后端访问失败返回 `50004`。
    pub async fn lock(
        &self,
        key: &str,
        ttl: Option<Duration>,
    ) -> AppResult<Option<DistributedLock>> {
        let namespaced = self.class_key(Class::Lock, key)?;
        let token = lock::next_token();
        let ttl = lock::effective_ttl(ttl, self.config.lock.default_ttl)?;

        if self.config.backend == CacheBackend::Redis {
            #[cfg(feature = "cache-redis")]
            if let Some(redis) = &self.redis {
                return Ok(redis
                    .try_lock(&namespaced, &token, ttl)
                    .await?
                    .then(|| DistributedLock::redis(redis.clone(), namespaced, token)));
            }
            // 配置要求跨实例互斥，但没有 Redis 连接：宁可失败，也不能静默用本地锁
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "[cache].backend = \"redis\" 但当前实例没有 Redis 连接，拒绝退化为进程内锁",
            ));
        }

        Ok(
            lock::try_acquire_memory(&self.locks, &namespaced, &token, ttl)
                .then(|| DistributedLock::memory(self.locks.clone(), namespaced, token)),
        )
    }

    /// 回源 + 回填（不参与 singleflight）。
    async fn load_and_fill<F, Fut>(
        &self,
        namespaced: &str,
        ttl: Option<Duration>,
        loader: F,
    ) -> AppResult<Option<Vec<u8>>>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = AppResult<Option<Vec<u8>>>> + Send,
    {
        let loaded = loader().await?;
        self.fill(namespaced, &loaded, ttl).await;
        Ok(loaded)
    }

    /// 回填（失败只告警）。
    async fn fill(&self, namespaced: &str, loaded: &Option<Vec<u8>>, ttl: Option<Duration>) {
        let (payload, effective_ttl) = match loaded {
            Some(bytes) => {
                if self.check_value_size(bytes.len()).is_err() {
                    tracing::warn!(key = %namespaced, len = bytes.len(), "回源值超过上限，跳过回填");
                    return;
                }
                (encode_value(bytes), ttl.unwrap_or(self.config.default_ttl))
            }
            None => {
                // 防穿透：可选的空值缓存（`[cache].null_cache_ttl > 0` 时启用）。
                if self.config.null_cache_ttl.is_zero() {
                    return;
                }
                (encode_null(), self.config.null_cache_ttl)
            }
        };

        let Ok(effective_ttl) = self.effective_ttl(Some(effective_ttl)) else {
            tracing::warn!(key = %namespaced, "TTL 非法，跳过回填");
            return;
        };
        if let Err(error) = self
            .backend
            .set(namespaced, &payload, Some(effective_ttl))
            .await
        {
            tracing::warn!(%error, key = %namespaced, "缓存回填失败（忽略）");
        }
    }

    /// 解析 TTL：未指定时取默认值，零值一律拒绝，再叠加抖动。
    fn effective_ttl(&self, ttl: Option<Duration>) -> AppResult<Duration> {
        let ttl = ttl.unwrap_or(self.config.default_ttl);
        if ttl.is_zero() {
            return Err(AppError::invalid_param("缓存 TTL 必须大于 0"));
        }
        Ok(self.with_jitter(ttl))
    }

    /// 防雪崩：给 TTL 加 `[cache].ttl_jitter` 范围内的随机抖动。
    ///
    /// 说明：`rand` 仅在 `security` feature 下引入，这里用哈希器随机种子 +
    /// 纳秒时间构造抖动，不为抖动单独引入依赖。
    fn with_jitter(&self, ttl: Duration) -> Duration {
        let jitter = self.config.ttl_jitter;
        if jitter.is_zero() {
            return ttl;
        }
        let bound = u64::try_from(jitter.as_nanos()).unwrap_or(u64::MAX).max(1);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(ttl.as_nanos());
        hasher.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
        );
        let extra = Duration::from_nanos(hasher.finish() % bound);
        ttl.saturating_add(extra)
    }

    /// 取得某 key 的 singleflight 闸门（原子建闸，避免并发各建一个）。
    fn inflight_entry(&self, namespaced: &str) -> Arc<Inflight> {
        self.inflight
            .entry(namespaced.to_string())
            .or_insert_with(|| Arc::new(Inflight::new()))
            .into_value()
    }
}

/// 编码：有值 → `[1][payload]`。
fn encode_value(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 1);
    out.push(TAG_VALUE);
    out.extend_from_slice(value);
    out
}

/// 编码：确认不存在 → `[0]`。
fn encode_null() -> Vec<u8> {
    vec![TAG_NULL]
}

/// 解码：`[0]` → `None`；`[1][payload]` → `Some(payload)`。
fn decode(mut bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    let tag = bytes.remove(0);
    if tag == TAG_VALUE { Some(bytes) } else { None }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::config::sections::cache::{LimitsSection, RedisSection};

    /// 记录写入键的后端（用于断言键布局）。
    #[derive(Default)]
    struct RecordingCache {
        keys: Mutex<Vec<String>>,
        inner: MemoryCache,
    }

    #[async_trait]
    impl Cache for RecordingCache {
        async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
            self.inner.get(key).await
        }
        async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> AppResult<()> {
            self.keys.lock().unwrap().push(key.to_string());
            self.inner.set(key, value, ttl).await
        }
        async fn del(&self, key: &str) -> AppResult<()> {
            self.inner.del(key).await
        }
        async fn take(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
            self.inner.take(key).await
        }
        async fn set_nx(&self, key: &str, value: &[u8], ttl: Duration) -> AppResult<bool> {
            self.inner.set_nx(key, value, ttl).await
        }
        async fn del_if_eq(&self, key: &str, expected: &[u8]) -> AppResult<bool> {
            self.inner.del_if_eq(key, expected).await
        }
    }

    fn service() -> CacheService {
        CacheService::new(Arc::new(MemoryCache::default()), CacheSection::default())
    }

    #[tokio::test]
    async fn should_roundtrip_through_namespace() {
        let service = service();
        service.set("k", b"v", None).await.unwrap();
        assert_eq!(service.get("k").await.unwrap().as_deref(), Some(&b"v"[..]));
        assert!(service.exists("k").await.unwrap());
        service.del("k").await.unwrap();
        assert!(!service.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn should_prefix_value_class() {
        let backend = Arc::new(RecordingCache::default());
        let service = CacheService::new(backend.clone(), CacheSection::default());
        service.set("session:abc", b"v", None).await.unwrap();
        let keys = backend.keys.lock().unwrap().clone();
        assert_eq!(keys, vec!["aurora:c:session:abc".to_string()]);
    }

    #[tokio::test]
    async fn should_isolate_semantic_classes_from_value_keys() {
        let service = service();
        service.set("session:abc", b"value", None).await.unwrap();
        let session = service.secure(Class::Session);
        session
            .put("abc", b"session", Duration::from_secs(60))
            .await
            .unwrap();

        assert_eq!(
            service.get("session:abc").await.unwrap().as_deref(),
            Some(&b"value"[..])
        );
        assert_eq!(
            session.get("abc").await.unwrap().as_deref(),
            Some(&b"session"[..])
        );
    }

    #[tokio::test]
    async fn should_reject_zero_ttl() {
        let service = service();
        assert!(service.set("k", b"v", Some(Duration::ZERO)).await.is_err());
    }

    #[tokio::test]
    async fn should_roundtrip_empty_payload() {
        let service = service();
        service.set("k", b"", None).await.unwrap();
        assert_eq!(service.get("k").await.unwrap().as_deref(), Some(&b""[..]));
    }

    #[tokio::test]
    async fn should_not_confuse_user_bytes_with_null_marker() {
        let service = service();
        let payload = b"\x00aurora:null".to_vec();
        service.set("k", &payload, None).await.unwrap();
        assert_eq!(service.get("k").await.unwrap(), Some(payload));
    }

    #[tokio::test]
    async fn should_load_and_backfill_on_miss() {
        let service = service();
        let calls = AtomicUsize::new(0);
        let value = service
            .get_or_set("k", None, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(b"loaded".to_vec()))
            })
            .await
            .unwrap();

        assert_eq!(value.as_deref(), Some(&b"loaded"[..]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let cached = service
            .get_or_set("k", None, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(b"again".to_vec()))
            })
            .await
            .unwrap();
        assert_eq!(cached.as_deref(), Some(&b"loaded"[..]), "应命中缓存");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn should_coalesce_concurrent_loads_when_singleflight_enabled() {
        let service = service();
        let calls = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let service = service.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .get_or_set("hot", None, || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(Some(b"v".to_vec()))
                    })
                    .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "并发回源应合并为一次（防击穿）"
        );
    }

    #[tokio::test]
    async fn should_share_confirmed_missing_between_waiters() {
        let service = service();
        let calls = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let service = service.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .get_or_set("missing", None, || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(None)
                    })
                    .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().unwrap().is_none());
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "「确认不存在」也必须共享，否则等待者会串行重跑 loader"
        );
    }

    #[tokio::test]
    async fn should_not_share_errors_by_default() {
        let service = service();
        let calls = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let service = service.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .get_or_set("boom", None, || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(AppError::internal(anyhow::anyhow!("boom")))
                    })
                    .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_err());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "错误默认不共享：每个请求各自回源（避免瞬时故障放大为整批失败）"
        );
    }

    #[tokio::test]
    async fn should_not_coalesce_when_singleflight_disabled() {
        let mut config = CacheSection::default();
        config.singleflight.enabled = false;
        let service = CacheService::new(Arc::new(MemoryCache::default()), config);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let service = service.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .get_or_set("hot", None, || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok(Some(b"v".to_vec()))
                    })
                    .await
            }));
        }
        for task in tasks {
            let _ = task.await.unwrap();
        }

        assert!(
            calls.load(Ordering::SeqCst) > 1,
            "关闭 singleflight 后并发应各自回源"
        );
    }

    #[tokio::test]
    async fn should_not_cache_null_when_disabled() {
        let service = service();
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            let value = service
                .get_or_set("missing", None, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
                .await
                .unwrap();
            assert!(value.is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "默认不缓存空值");
    }

    #[tokio::test]
    async fn should_cache_null_when_enabled() {
        let config = CacheSection {
            null_cache_ttl: Duration::from_secs(30),
            ..CacheSection::default()
        };
        let service = CacheService::new(Arc::new(MemoryCache::default()), config);

        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            let value = service
                .get_or_set("missing", None, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
                .await
                .unwrap();
            assert!(value.is_none(), "命中空值缓存仍应返回 None");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "空值应被缓存（防穿透）");
    }

    #[tokio::test]
    async fn should_acquire_lock_exclusively_and_release_on_drop() {
        let service = service();
        let first = service
            .lock("job", None)
            .await
            .unwrap()
            .expect("首次应获取到锁");
        assert!(
            service.lock("job", None).await.unwrap().is_none(),
            "已占用时应返回 None"
        );
        drop(first);
        assert!(
            service.lock("job", None).await.unwrap().is_some(),
            "drop 后可再获取"
        );
    }

    #[tokio::test]
    async fn should_refuse_memory_lock_when_redis_backend_configured() {
        let config = CacheSection {
            backend: CacheBackend::Redis,
            redis: RedisSection {
                url: Some("redis://127.0.0.1:6379".to_string()),
                ..RedisSection::default()
            },
            ..CacheSection::default()
        };
        let service = CacheService::new(Arc::new(MemoryCache::default()), config);
        let error = service.lock("job", None).await.expect_err("不得静默降级");
        assert_eq!(error.code(), ErrorCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn should_decode_envelope() {
        assert!(decode(encode_null()).is_none());
        assert_eq!(decode(encode_value(b"v")).as_deref(), Some(&b"v"[..]));
        assert_eq!(decode(encode_value(b"")).as_deref(), Some(&b""[..]));
        assert!(decode(Vec::new()).is_none(), "空值视为未命中");
    }

    #[test]
    fn should_expose_limits_from_config() {
        let config = CacheSection {
            limits: LimitsSection {
                max_value_bytes: 4,
                ..LimitsSection::default()
            },
            ..CacheSection::default()
        };
        let service = CacheService::new(Arc::new(MemoryCache::default()), config);
        assert!(service.check_value_size(4).is_ok());
        assert_eq!(
            service.check_value_size(5).unwrap_err().code(),
            ErrorCode::PAYLOAD_TOO_LARGE
        );
    }
}
