//! 限流服务：按 `[ratelimit]` 编译出「默认桶 + 按路由覆盖的桶」，并暴露检查入口。
//!
//! ## 为什么是「一组桶」而不是「一个桶」
//!
//! 全局只有一组 `limit/window` 时，登录接口和静态资源共用同一个配额：
//! 默认 100 次/60s 意味着单 IP 每分钟可以做 100 次口令尝试（14.4 万次/天），
//! 撞库与暴力破解基本无阻力。`[[ratelimit.rules]]` 让敏感端点能单独收紧，
//! 未命中的路径仍走默认桶。
//!
//! 规则匹配顺序：**路径段前缀最长者优先**（`/auth/login` 先于 `/auth`），
//! 都未命中则走默认桶。前缀按**路径段**比较——`/auth` 不会命中 `/authorize`。

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Extensions, HeaderMap, Method};

use crate::config::RateLimitSection;
use crate::config::sections::ratelimit::{RateLimitStore as StoreKind, RateLimitStrategy};
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::ratelimit::key::{KeyHasher, Keys, UNKNOWN_KEY, keys_for};
use crate::ratelimit::store::{MemoryStore, RateLimitStore};
use crate::ratelimit::throttle::ThrottledWarn;
use crate::utils::ProxyTrust;

#[cfg(feature = "ratelimit-redis")]
use crate::ratelimit::RedisRateLimitStore;

/// 限流服务（`AppState.rate_limit`）。
#[derive(Clone)]
pub struct RateLimitService {
    config: RateLimitSection,
    /// 已编译的桶（含默认桶，默认桶的 `prefix` 为空）
    rules: Arc<[Bucket]>,
    /// 默认桶在 `rules` 中的下标（排序后位置会变）
    default: usize,
    hasher: Arc<KeyHasher>,
    excludes: Arc<[String]>,
    /// `[ratelimit].unknown_limit > 0` 时的兜底桶
    unknown: Option<Arc<dyn RateLimitStore>>,
    unknown_warn: Arc<ThrottledWarn>,
}

/// 一个计数桶（`pub(crate)`：中间件按它取窗口 / 维度）。
pub(crate) struct Bucket {
    name: String,
    /// 路径段前缀；默认桶为空串
    prefix: String,
    methods: Vec<Method>,
    strategy: RateLimitStrategy,
    store: Arc<dyn RateLimitStore>,
    /// `per_key` + `[ratelimit].ip_limit > 0` 时叠加的 IP 桶
    ip_store: Option<Arc<dyn RateLimitStore>>,
    window: Duration,
}

impl std::fmt::Debug for RateLimitService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimitService")
            .field("enabled", &self.config.enabled)
            .field("strategy", &self.config.strategy)
            .field("limit", &self.config.limit)
            .field("window", &self.config.window)
            .field("store", &self.config.store)
            .field("rules", &self.rules.len())
            .field("exclude_paths", &self.excludes)
            .finish()
    }
}

impl RateLimitService {
    /// 按 `[ratelimit]` 配置创建。
    ///
    /// `redis_url` 仅当 `[ratelimit].store = redis` 时需要（复用 `[cache].redis.url`）；
    /// `namespace` 取 `[cache].namespace`，用于在共享 Redis DB 里隔离键空间。
    ///
    /// # Errors
    /// `store = redis` 但缺 url 或未启用 `ratelimit-redis` feature 时返回错误。
    pub async fn from_config(
        config: &RateLimitSection,
        redis_url: Option<&str>,
        namespace: &str,
    ) -> AppResult<Self> {
        let backend = Backend::build(config, redis_url, namespace).await?;
        let hasher = Arc::new(KeyHasher::new(&config.key_salt));

        // 默认桶 + 每条规则一个桶
        let mut rules: Vec<Bucket> = Vec::with_capacity(config.rules.len() + 1);
        rules.push(Bucket::compile(
            "default".to_string(),
            String::new(),
            Vec::new(),
            config.strategy,
            config.limit,
            config.window,
            &backend,
            config.ip_limit,
        ));

        for (index, rule) in config.rules.iter().enumerate() {
            // `path = "/"` 表示「命中一切」，但不能和默认桶一样用空前缀，
            // 否则排序后无法区分「默认桶」和这条规则
            let prefix = match trim_prefix(&rule.path) {
                trimmed if trimmed.is_empty() => "/".to_string(),
                trimmed => trimmed,
            };
            rules.push(Bucket::compile(
                format!("rule{index}"),
                prefix,
                rule.methods
                    .iter()
                    .filter_map(|method| {
                        Method::from_bytes(method.trim().to_uppercase().as_bytes()).ok()
                    })
                    .collect(),
                rule.strategy.unwrap_or(config.strategy),
                rule.limit,
                rule.window.unwrap_or(config.window),
                &backend,
                config.ip_limit,
            ));
        }

        // 最长前缀优先（稳定排序：同长度保持配置顺序）
        rules.sort_by_key(|bucket| std::cmp::Reverse(bucket.prefix.len()));
        let default = rules
            .iter()
            .position(|bucket| bucket.prefix.is_empty())
            .unwrap_or(0);

        let unknown = (config.unknown_limit > 0)
            .then(|| backend.store("unknown", config.unknown_limit, config.window));

        if hasher.is_ephemeral()
            && config.store == StoreKind::Redis
            && config.strategy == RateLimitStrategy::PerKey
        {
            tracing::warn!(
                "[ratelimit].key_salt 为空：主体桶的哈希盐是每进程随机的，多实例之间桶不共享\
                 （实际配额会放大到 limit × 副本数）。跨实例共享请显式配置同一个 key_salt"
            );
        }
        #[cfg(feature = "session")]
        if config.enabled && config.strategy == RateLimitStrategy::PerKey {
            tracing::warn!(
                "[ratelimit].strategy = \"per_key\" 只识别 Bearer JWT；启用 session 认证时\
                 cookie 请求拿不到主体，会退化成按 IP 限流。需要覆盖这类请求请叠加 \
                 `[ratelimit].ip_limit`，或在网关层按会话限流"
            );
        }

        Ok(Self {
            config: config.clone(),
            rules: rules.into(),
            default,
            hasher,
            excludes: config.exclude_paths.clone().into(),
            unknown,
            unknown_warn: Arc::new(ThrottledWarn::new(Duration::from_secs(30))),
        })
    }

    /// 追加额外豁免路径（指标端点由 `boot` 在启动时注入）。
    #[must_use]
    pub fn with_extra_excludes<I>(mut self, extra: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let mut excludes: Vec<String> = self.excludes.to_vec();
        for path in extra {
            if !path.is_empty() && !excludes.contains(&path) {
                excludes.push(path);
            }
        }
        self.excludes = excludes.into();
        self
    }

    /// 是否启用（`[ratelimit].enabled`）。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// 默认桶的限流维度。
    #[must_use]
    pub fn strategy(&self) -> RateLimitStrategy {
        self.config.strategy
    }

    /// 默认桶的计数窗口（`Retry-After` 的上限参考）。
    #[must_use]
    pub fn window(&self) -> Duration {
        self.rules[self.default].window
    }

    /// 路径是否豁免限流（健康检查 / 指标端点）。
    #[must_use]
    pub fn is_excluded(&self, path: &str) -> bool {
        self.excludes
            .iter()
            .any(|prefix| matches_prefix(path, prefix))
    }

    /// 为本次请求选桶（最长路径段前缀优先，未命中走默认桶）。
    #[must_use]
    pub(crate) fn bucket_for(&self, path: &str, method: &Method) -> &Bucket {
        self.rules
            .iter()
            .find(|bucket| {
                !bucket.prefix.is_empty()
                    && matches_prefix(path, &bucket.prefix)
                    && bucket.accepts(method)
            })
            .unwrap_or(&self.rules[self.default])
    }

    /// 提取限流键（见 `key::keys_for`）。
    ///
    /// `subject` 必须是**已验签**的主体：本层在 `auth` 外侧，客户端自带的任何
    /// 凭据头都不能单独构成计数桶（否则换个头就换个桶）。
    #[must_use]
    pub(crate) fn keys_for(
        &self,
        headers: &HeaderMap,
        extensions: &Extensions,
        trust: ProxyTrust<'_>,
        strategy: RateLimitStrategy,
        subject: Option<&str>,
    ) -> Keys {
        keys_for(headers, extensions, strategy, trust, subject, &self.hasher)
    }

    /// 按桶检查并消耗额度。
    ///
    /// 双桶（`per_key` + `ip_limit`）时两个桶都要通过：主体桶限制单个账号，
    /// IP 桶兜住「一个账号换多个 IP」与「一个 IP 换多个账号」。
    pub(crate) async fn check_bucket(&self, bucket: &Bucket, keys: &Keys) -> Result<(), Duration> {
        // 来源不可知：不建桶（键会全部塌进同一个共享桶），走独立兜底配额
        if keys.is_unknown() {
            return self.check_unknown().await;
        }

        let primary = bucket.store.check(keys.primary()).await;

        // 主体桶命中时再叠一道 IP 桶；未验签时主键本身就是 IP，不能重复计数
        if keys.subject.is_some()
            && let (Some(ip_store), Some(ip)) = (bucket.ip_store.as_ref(), keys.ip.as_deref())
        {
            let ip_result = ip_store.check(ip).await;
            return match (primary, ip_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(wait), _) | (_, Err(wait)) => Err(wait),
            };
        }

        primary
    }

    /// 检查并消耗一个额度（默认桶；供自定义场景 / 测试直接调用）。
    ///
    /// `Ok(())` 放行；`Err(wait)` 表示建议等待时长。
    ///
    /// 存储故障（`redis`）时 **fail-open**：放行并告警，不把错误抛给调用方。
    pub async fn check(&self, key: &str) -> Result<(), Duration> {
        self.rules[self.default].store.check(key).await
    }

    /// 来源不可知时的处理。
    ///
    /// 默认**不计数**：`per_ip` 在「拿不到 IP」时无法按来源区分，用一个共享桶
    /// 硬限等于给攻击者一个「一键封禁全站」的开关（连探活一起 429）。配置了
    /// `unknown_limit` 才启用应急刹车。
    async fn check_unknown(&self) -> Result<(), Duration> {
        let Some(store) = self.unknown.as_ref() else {
            if let Some(suppressed) = self.unknown_warn.hit() {
                tracing::warn!(
                    suppressed,
                    "限流来源不可知（请求缺少 ConnectInfo），已跳过按来源计数；\
                     生产环境请使用 `boot::serve` 注入对端地址"
                );
            }
            return Ok(());
        };
        store.check(UNKNOWN_KEY).await
    }
}

/// 存储后端：memory 每个桶独立实例；redis 共享一条连接、按规则前缀隔离。
enum Backend {
    Memory {
        max_keys: usize,
    },
    #[cfg(feature = "ratelimit-redis")]
    Redis(RedisRateLimitStore),
}

impl Backend {
    async fn build(
        config: &RateLimitSection,
        redis_url: Option<&str>,
        namespace: &str,
    ) -> AppResult<Self> {
        match config.store {
            StoreKind::Memory => Ok(Self::Memory {
                max_keys: config.max_keys,
            }),
            StoreKind::Redis => {
                #[cfg(feature = "ratelimit-redis")]
                {
                    let url = redis_url.ok_or_else(|| {
                        AppError::biz(
                            ErrorCode::SERVICE_UNAVAILABLE,
                            "[ratelimit].store = redis 需要配置 [cache].redis.url\
                             （AURORA_CACHE__REDIS__URL）",
                        )
                    })?;
                    Ok(Self::Redis(
                        RedisRateLimitStore::connect(url, namespace, config.limit, config.window)
                            .await?,
                    ))
                }
                #[cfg(not(feature = "ratelimit-redis"))]
                {
                    let _ = (redis_url, namespace);
                    Err(AppError::biz(
                        ErrorCode::SERVICE_UNAVAILABLE,
                        "[ratelimit].store = redis 需要启用 `ratelimit-redis` feature",
                    ))
                }
            }
        }
    }

    fn store(&self, name: &str, limit: u32, window: Duration) -> Arc<dyn RateLimitStore> {
        // memory 后端每个桶是独立实例，不需要桶名；redis 用它隔离键前缀
        let _ = name;
        match self {
            Self::Memory { max_keys } => {
                Arc::new(MemoryStore::with_max_keys(limit, window, *max_keys))
            }
            #[cfg(feature = "ratelimit-redis")]
            Self::Redis(root) => Arc::new(root.derive(name, limit, window)),
        }
    }
}

impl Bucket {
    #[allow(clippy::too_many_arguments)]
    fn compile(
        name: String,
        prefix: String,
        methods: Vec<Method>,
        strategy: RateLimitStrategy,
        limit: u32,
        window: Duration,
        backend: &Backend,
        ip_limit: u32,
    ) -> Self {
        let store = backend.store(&name, limit, window);
        // 双桶只对 per_key 有意义：per_ip 的主键本身就是 IP
        let ip_store = (strategy == RateLimitStrategy::PerKey && ip_limit > 0)
            .then(|| backend.store(&format!("{name}:ip"), ip_limit, window));
        Self {
            name,
            prefix,
            methods,
            strategy,
            store,
            ip_store,
            window,
        }
    }

    pub(crate) fn strategy(&self) -> RateLimitStrategy {
        self.strategy
    }

    pub(crate) fn window(&self) -> Duration {
        self.window
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    fn accepts(&self, method: &Method) -> bool {
        self.methods.is_empty() || self.methods.iter().any(|allowed| allowed == method)
    }
}

/// 去掉尾部 `/`（根路径除外），便于按路径段比较。
fn trim_prefix(prefix: &str) -> String {
    if prefix == "/" {
        return String::new();
    }
    prefix.trim_end_matches('/').to_string()
}

/// 路径段前缀匹配：`/auth` 命中 `/auth`、`/auth/login`，但**不**命中 `/authorize`。
///
/// 用裸 `starts_with` 会让 `/auth` 意外命中 `/authorize`——给敏感端点配的严格
/// 配额被一个毫不相干的路由蹭掉，反过来也可能让本该受限的路由漏出配额。
fn matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if prefix == "/" {
        return path.starts_with('/');
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sections::ratelimit::{RateLimitRule, RateLimitStore};

    fn config(enabled: bool, limit: u32) -> RateLimitSection {
        RateLimitSection {
            enabled,
            limit,
            ..RateLimitSection::default()
        }
    }

    async fn service(config: &RateLimitSection) -> RateLimitService {
        RateLimitService::from_config(config, None, "test")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn should_report_enabled_flag() {
        assert!(!service(&config(false, 10)).await.enabled());
        assert!(service(&config(true, 10)).await.enabled());
    }

    #[tokio::test]
    async fn should_enforce_limit() {
        let service = service(&config(true, 2)).await;
        assert!(service.check("k").await.is_ok());
        assert!(service.check("k").await.is_ok());
        assert!(service.check("k").await.is_err());
    }

    #[test]
    fn should_default_store_to_memory() {
        assert_eq!(RateLimitSection::default().store, RateLimitStore::Memory);
    }

    #[tokio::test]
    async fn should_exclude_health_by_default() {
        let service = service(&config(true, 10)).await;
        assert!(service.is_excluded("/health"));
        assert!(service.is_excluded("/ready"));
        assert!(!service.is_excluded("/api/users"));
    }

    #[tokio::test]
    async fn should_append_extra_excludes() {
        let service = service(&config(true, 10))
            .await
            .with_extra_excludes(["/metrics".to_string()]);
        assert!(service.is_excluded("/metrics"));
        assert!(service.is_excluded("/health"), "默认豁免不能丢");
    }

    /// 前缀必须按路径段比较：`/auth` 不得命中 `/authorize`。
    #[test]
    fn should_match_prefix_by_path_segment() {
        assert!(matches_prefix("/auth", "/auth"));
        assert!(matches_prefix("/auth/login", "/auth"));
        assert!(!matches_prefix("/authorize", "/auth"));
        assert!(!matches_prefix("/authentication", "/auth"));
        assert!(matches_prefix("/anything", ""), "空前缀 = 全匹配（默认桶）");
    }

    #[tokio::test]
    async fn should_route_to_longest_matching_rule() {
        let mut section = config(true, 100);
        section.rules = vec![
            RateLimitRule {
                path: "/auth".to_string(),
                methods: vec![],
                strategy: None,
                limit: 5,
                window: None,
            },
            RateLimitRule {
                path: "/auth/login".to_string(),
                methods: vec!["POST".to_string()],
                strategy: None,
                limit: 2,
                window: None,
            },
        ];
        let service = service(&section).await;

        // 最长前缀优先
        let bucket = service.bucket_for("/auth/login", &Method::POST);
        assert_eq!(bucket.window(), Duration::from_secs(60));
        assert_eq!(
            service.bucket_for("/auth/refresh", &Method::POST).name(),
            "rule0",
            "未命中 /auth/login 时应落到 /auth"
        );
        // 方法不匹配 → 回落到更短前缀
        assert_eq!(
            service.bucket_for("/auth/login", &Method::GET).name(),
            "rule0"
        );
        // 都不命中 → 默认桶
        assert_eq!(
            service.bucket_for("/api/users", &Method::GET).name(),
            "default"
        );
    }

    /// 敏感端点必须能真的用上**自己**的配额。
    #[tokio::test]
    async fn should_enforce_per_route_limit() {
        let mut section = config(true, 100);
        section.rules = vec![RateLimitRule {
            path: "/auth".to_string(),
            methods: vec!["POST".to_string()],
            strategy: None,
            limit: 2,
            window: None,
        }];
        let service = service(&section).await;

        let keys = Keys {
            subject: None,
            ip: Some("ip:203.0.113.9".to_string()),
        };
        let bucket = service.bucket_for("/auth/login", &Method::POST);
        assert!(service.check_bucket(bucket, &keys).await.is_ok());
        assert!(service.check_bucket(bucket, &keys).await.is_ok());
        assert!(
            service.check_bucket(bucket, &keys).await.is_err(),
            "登录接口应在 2 次后被限流，而不是 100 次"
        );

        // 默认桶不受影响
        let default = service.bucket_for("/api/users", &Method::GET);
        assert!(service.check_bucket(default, &keys).await.is_ok());
    }

    /// 来源不可知默认不计数（只告警），不能变成「一键封禁全站」的开关。
    #[tokio::test]
    async fn should_not_count_unknown_source_by_default() {
        let service = service(&config(true, 1)).await;
        let keys = Keys::default();
        assert!(keys.is_unknown());
        for _ in 0..10 {
            assert!(service.check_unknown().await.is_ok());
        }
    }

    /// 配了 `unknown_limit` 才有应急刹车。
    #[tokio::test]
    async fn should_enforce_unknown_limit_when_configured() {
        let mut section = config(true, 1);
        section.unknown_limit = 2;
        let service = service(&section).await;
        assert!(service.check_unknown().await.is_ok());
        assert!(service.check_unknown().await.is_ok());
        assert!(service.check_unknown().await.is_err());
    }

    /// 双桶：主体桶与 IP 桶都要过，任一触发即拒。
    #[tokio::test]
    async fn should_enforce_both_subject_and_ip_buckets() {
        let mut section = config(true, 1);
        section.strategy = RateLimitStrategy::PerKey;
        section.ip_limit = 2;
        let service = service(&section).await;

        let mut keys = Keys {
            subject: Some("sub:aaaa".to_string()),
            ip: Some("ip:203.0.113.9".to_string()),
        };
        // 主体桶 limit = 1
        assert!(
            service
                .check_bucket(service.bucket_for("/x", &Method::GET), &keys)
                .await
                .is_ok()
        );

        // 换个主体，同一 IP：IP 桶还有额度
        keys.subject = Some("sub:bbbb".to_string());
        assert!(
            service
                .check_bucket(service.bucket_for("/x", &Method::GET), &keys)
                .await
                .is_ok()
        );

        // 再换主体，同一 IP：IP 桶（limit = 2）已满 → 拒绝
        keys.subject = Some("sub:cccc".to_string());
        assert!(
            service
                .check_bucket(service.bucket_for("/x", &Method::GET), &keys)
                .await
                .is_err(),
            "同一 IP 换主体也要受限"
        );
    }
}
