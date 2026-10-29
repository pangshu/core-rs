//! 应用装配。
//!
//! [`AppState::build`] 对应启动状态机第 3 / 6 步：连接资源（并 ping）→ 构建
//! `AppState`。[`AppState::build_with_migrator`] 额外覆盖第 4 步（迁移）。
//!
//! 资源连接失败即返回对应的 [`StartupError`]，由调用方以非零码退出。

use std::time::Duration;

use crate::app::lifecycle::LifecycleHooks;
use crate::app::state::AppState;
#[cfg(feature = "auth")]
use crate::auth::AuthService;
use crate::boot::startup::StartupError;
#[cfg(feature = "cache")]
use crate::cache::{CacheHealth, CacheService};
use crate::config::{AppConfig, ConfigHandle};
#[cfg(feature = "db")]
use crate::db;
#[cfg(feature = "geoip")]
use crate::geoip::{GeoIp, GeoIpHealth};
#[cfg(feature = "i18n")]
use crate::i18n::I18n;
#[cfg(feature = "queue")]
use crate::queue::{QueueHealth, QueueService};
#[cfg(feature = "ratelimit")]
use crate::ratelimit::RateLimitService;
#[cfg(feature = "resilience")]
use crate::resilience::PolicyRegistry;
#[cfg(feature = "task")]
use crate::scheduler::Scheduler;
#[cfg(feature = "security")]
use crate::security::SecurityContext;
use crate::telemetry::HealthRegistry;
#[cfg(feature = "metrics")]
use crate::telemetry::Metrics;

impl AppState {
    /// 按启用的 feature 装配应用状态（连接资源并 ping，不执行迁移）。
    ///
    /// **不启动配置热更新**：热更新要监听哪些文件是应用层的知识，需要
    /// [`ConfigSource`](crate::config::ConfigSource)。想按 `[watch].enabled` 自动开启，
    /// 用 [`Self::build_with_source`]。
    ///
    /// # Errors
    /// 资源连接或初始化失败时返回对应的 [`StartupError`]。
    pub async fn build(config: &AppConfig) -> Result<Self, StartupError> {
        Self::build_inner(config, None).await
    }

    /// 装配应用状态，并在 `[watch].enabled = true` 时用 `source` 启动配置热更新。
    ///
    /// `source` 必须与启动加载用的是**同一个**：热更新会重放它，否则会出现
    /// "启动读 A 文件、热更新读 B 文件"。
    ///
    /// # Errors
    /// 资源连接或初始化失败时返回对应的 [`StartupError`]；热更新启动失败**不阻断**
    /// 启动（只告警）。
    #[cfg(feature = "watch")]
    pub async fn build_with_source(
        config: &AppConfig,
        source: crate::config::ConfigSource,
    ) -> Result<Self, StartupError> {
        Self::build_inner(config, Some(source)).await
    }

    async fn build_inner(
        config: &AppConfig,
        #[cfg_attr(not(feature = "watch"), allow(unused_variables))] source: Option<
            crate::config::ConfigSource,
        >,
    ) -> Result<Self, StartupError> {
        // 启动日志（状态机第 6 步，此时 telemetry 已初始化）：只记录环境标识，不输出配置内容
        tracing::info!(
            app = %config.app.name,
            env = %config.env,
            production = config.is_production(),
            "配置已加载"
        );
        // 传输层加固提示：应用层没有请求头读超时，必须由前置代理承担（见函数文档）
        crate::boot::startup::warn_transport_hardening(config);
        let config_handle = ConfigHandle::new(config.clone());
        // 探针超时独立于请求超时，且小于常见探测周期（见 docs/02 §8）。
        let health = HealthRegistry::with_timeout(probe_timeout(config));

        // 认证（feature auth）：JWT 编解码器
        #[cfg(feature = "auth")]
        let auth = AuthService::from_config(&config.auth)
            .await
            .map_err(|e| StartupError::Config(format!("auth init failed: {e}")))?;

        // 授权（feature authz）：RBAC 策略引擎。**必须在这里接上**，
        // 否则 `enforce` / `require` 一律拒绝（fail-closed，不是放行）。
        #[cfg(all(feature = "auth", feature = "authz"))]
        let auth = auth
            .with_authorizer_from_config(&config.authz)
            .await
            .map_err(|e| StartupError::Config(format!("authz init failed: {e}")))?;

        // 会话存储：`[auth].session.store` = memory / redis（多实例共享）
        #[cfg(all(feature = "auth", feature = "session-redis"))]
        let auth = if config.auth.session.store
            == crate::config::sections::auth::SessionStoreKind::Redis
        {
            let url = config.cache.redis.url.as_deref().ok_or_else(|| {
                StartupError::Config(
                    "auth.session.store = redis 需要配置 [cache].redis.url\
                     （AURORA_CACHE__REDIS__URL）"
                        .to_string(),
                )
            })?;
            let store = crate::auth::RedisSessionStore::connect(url, &config.cache.namespace)
                .await
                .map_err(|e| {
                    StartupError::Resource(format!(
                        "session store init failed: {}",
                        crate::utils::string::redact_url_secrets(&e.to_string())
                    ))
                })?;
            auth.with_session_store(std::sync::Arc::new(store))
        } else {
            auth
        };

        // 启动状态机第 3 步：连接资源并 ping
        #[cfg(feature = "db")]
        let db = {
            let db = db::connect(&config.database).await?;
            db::ping(&db).await.map_err(|e| {
                StartupError::Resource(format!(
                    "database ping failed: {}",
                    crate::utils::string::redact_url_secrets(&e.to_string())
                ))
            })?;
            health.register(db::DbHealth::new(db.clone()));
            db
        };

        // 缓存（feature cache）：按 `[cache].backend` 装配，并注册 `/ready` 探针
        #[cfg(feature = "cache")]
        let cache = {
            let cache = CacheService::from_config(&config.cache)
                .await
                .map_err(|e| {
                    StartupError::Resource(format!(
                        "cache init failed: {}",
                        crate::utils::string::redact_url_secrets(&e.to_string())
                    ))
                })?;
            health.register(CacheHealth::new(cache.clone()));
            cache
        };

        // 队列（feature queue）：按 `[queue].backend` 装配，并注册 `/ready` 探针
        #[cfg(feature = "queue")]
        let queue = {
            let queue = QueueService::from_config(&config.queue)
                .await
                .map_err(|e| {
                    StartupError::Resource(format!(
                        "queue init failed: {}",
                        crate::utils::string::redact_url_secrets(&e.to_string())
                    ))
                })?;
            #[cfg(feature = "cache")]
            let queue = if config.queue.idempotency.enabled {
                queue.with_cache(cache.clone())
            } else {
                queue
            };
            #[cfg(not(feature = "cache"))]
            if config.queue.idempotency.enabled {
                tracing::warn!(
                    "[queue].idempotency.enabled = true 但未启用 cache feature，消费去重将被忽略"
                );
            }
            health.register(QueueHealth::new(queue.clone()));
            queue
        };

        // 出站治理策略（feature resilience）：纯内存构建，无外部依赖
        #[cfg(feature = "resilience")]
        let resilience = PolicyRegistry::from_config(&config.resilience);

        // 限流（feature ratelimit）：进程内 GCRA 计数；`enabled` 在请求期判定
        #[cfg(feature = "ratelimit")]
        let rate_limit = {
            // `store = redis` 时复用 `[cache].redis.url` 与 `[cache].namespace`
            #[cfg(feature = "cache")]
            let (redis_url, namespace) = (
                config.cache.redis.url.as_deref(),
                config.cache.namespace.as_str(),
            );
            #[cfg(not(feature = "cache"))]
            let (redis_url, namespace) = (None, "aurora");

            // 指标端点用**抓取令牌**而不是 JWT，且是探针流量：与业务共用配额时
            // 一次配额打满就会让 Prometheus 抓取全部 429，监控直接失明
            #[cfg(feature = "metrics")]
            let extra_excludes: Vec<String> = if config.telemetry.metrics.enabled {
                vec![config.telemetry.metrics.path.clone()]
            } else {
                Vec::new()
            };
            #[cfg(not(feature = "metrics"))]
            let extra_excludes: Vec<String> = Vec::new();

            RateLimitService::from_config(&config.ratelimit, redis_url, namespace)
                .await
                .map_err(|e| StartupError::Config(format!("ratelimit init failed: {e}")))?
                .with_extra_excludes(extra_excludes)
        };

        // IP 地理定位（feature geoip）：库缺失时降级运行（查询恒为 None），
        // 并注册 `/ready` 探针——只有「生产 + 未显式接受降级」时缺库才判不就绪。
        #[cfg(feature = "geoip")]
        let geoip = {
            let geoip = GeoIp::from_config(&config.geoip)
                .map_err(|e| StartupError::Config(format!("geoip init failed: {e}")))?;
            let required = config.is_production() && !config.geoip.allow_degraded;
            health.register(GeoIpHealth::new(geoip.clone(), required));
            geoip
        };

        // jti 黑名单（feature revocation）：登出 / 吊销后令牌立即失效
        // 放在 cache 装配之后：黑名单复用同一套 Redis 连接与命名空间。
        #[cfg(all(feature = "auth", feature = "revocation"))]
        let auth = if config.auth.revocation.enabled {
            auth.with_revocation(std::sync::Arc::new(crate::auth::RevocationStore::new(
                cache.clone(),
                config.auth.revocation.fail_closed,
            )))
            // 轮换与黑名单同源开关：都要写「安全状态」，也都复用同一套缓存连接。
            .with_rotation(std::sync::Arc::new(crate::auth::RefreshRotation::new(
                cache.clone(),
                config.auth.revocation.fail_closed,
            )))
        } else {
            auth
        };

        // 任务调度（feature task）：仅内存调度；distributed 时复用 cache 锁选主
        #[cfg(feature = "task")]
        let scheduler = Scheduler::from_config(&config.scheduler, cache.clone())
            .map_err(|e| StartupError::Config(format!("scheduler init failed: {e}")))?;

        // 国际化（feature i18n）：扫描资源目录确定受支持语言
        #[cfg(feature = "i18n")]
        let i18n = I18n::from_config(&config.i18n)
            .map_err(|e| StartupError::Config(format!("i18n init failed: {e}")))?;

        // 指标（feature metrics）：纯内存注册表，无外部依赖
        #[cfg(feature = "metrics")]
        let metrics = Metrics::new();

        // 配置热更新（feature watch）：`[watch].enabled` 且调用方给了 `ConfigSource`
        // 时才启动——框架不知道"该监听哪些文件"，那由应用层决定。
        #[cfg(feature = "watch")]
        let config_watcher = match (config.watch.enabled, source) {
            (true, None) => {
                tracing::warn!(
                    "[watch].enabled = true 但装配时未提供 ConfigSource：配置热更新未启动。\
                     请改用 AppState::build_with_source(config, source)"
                );
                None
            }
            (true, Some(source)) => {
                let result = crate::config::ConfigWatcher::start_with_callback(
                    config_handle.clone(),
                    source,
                    |new: &AppConfig| {
                        // 回调里做「需要重建的组件」；目前只有日志级别可以真热更新
                        if let Err(error) = crate::telemetry::set_level(&new.telemetry.level) {
                            tracing::warn!(%error, "日志级别热更新失败，保留原级别");
                        } else {
                            tracing::info!(level = %new.telemetry.level, "日志级别已热更新");
                        }
                    },
                );
                match result {
                    Ok(watcher) => Some(std::sync::Arc::new(watcher)),
                    Err(error) => {
                        tracing::warn!(%error, "配置热更新启动失败，继续以当前配置运行");
                        None
                    }
                }
            }
            (false, _) => None,
        };

        // 安全能力（feature security）：密钥留空 = 该能力未启用。
        // 启动期就把「密钥太短 / previous_key 单独出现 / Argon2 参数越界」挡下来，
        // 而不是等到第一次加密时才发现用的是空密钥。
        #[cfg(feature = "security")]
        let security = SecurityContext::from_config(&config.security)
            .map_err(|e| StartupError::Config(format!("security init failed: {e}")))?;

        Ok(Self {
            config: config_handle,
            health,
            lifecycle: LifecycleHooks::new(),
            #[cfg(feature = "auth")]
            auth,
            #[cfg(feature = "cache")]
            cache,
            #[cfg(feature = "queue")]
            queue,
            #[cfg(feature = "resilience")]
            resilience,
            #[cfg(feature = "ratelimit")]
            rate_limit,
            #[cfg(feature = "security")]
            security,
            #[cfg(feature = "i18n")]
            i18n,
            #[cfg(feature = "geoip")]
            geoip,
            #[cfg(feature = "task")]
            scheduler,
            #[cfg(feature = "metrics")]
            metrics,
            #[cfg(feature = "db")]
            db,
            #[cfg(feature = "watch")]
            config_watcher,
        })
    }
}

#[cfg(feature = "db")]
impl AppState {
    /// 装配应用状态，并在 `[database].migrate_on_start = true` 时执行迁移
    /// （启动状态机第 4 步，**绑定监听之前**）。
    ///
    /// `M` 为应用侧声明的 `Migrator`（见 `aurora::db::migrations`）。
    ///
    /// # Errors
    /// 资源连接或迁移失败时返回对应的 [`StartupError`]。
    pub async fn build_with_migrator<M>(config: &AppConfig) -> Result<Self, StartupError>
    where
        M: db::migrations::MigratorTrait,
    {
        let state = Self::build(config).await?;
        db::migrate_on_start::<M>(&state.db, config.database.migrate_on_start).await?;
        Ok(state)
    }
}

/// 由 `[server].request_timeout` 推导探针超时（上限 1s）。
fn probe_timeout(config: &AppConfig) -> Duration {
    let request_timeout = config.server.request_timeout;
    let capped = request_timeout.min(Duration::from_secs(1));
    if capped.is_zero() {
        Duration::from_millis(1)
    } else {
        capped
    }
}

#[cfg(all(test, feature = "auth", feature = "session-redis"))]
mod tests {
    use super::*;
    use crate::config::sections::auth::SessionStoreKind;

    #[tokio::test]
    async fn should_fail_when_redis_session_store_has_no_url() {
        let mut config = AppConfig::default();
        config.auth.session.store = SessionStoreKind::Redis;
        config.cache.redis.url = None;

        let error = AppState::build(&config).await.unwrap_err();
        assert!(
            matches!(error, StartupError::Config(_)),
            "缺少 redis url 应报配置错误，实际：{error:?}"
        );
        assert!(
            error.to_string().contains("redis.url"),
            "错误信息应指出缺哪个配置，实际：{error}"
        );
    }
}
