//! [`CoreState`]：框架提供的路由间共享上下文——db 池 / 缓存 / 锁 / 队列 /
//! 配置句柄（ConfigHandle）/ 认证链等。应用 `AppState` 内嵌它（文档 三·3）：
//!
//! ```rust,ignore
//! #[derive(Clone)]
//! pub struct AppState {
//!     pub core: CoreState,
//!     // 业务字段按需加：对象存储客户端、第三方服务客户端…
//! }
//! impl HasDb for AppState {
//!     fn db(&self) -> Option<&DatabaseConnection> { self.core.db() }
//! }
//! ```

use std::sync::Arc;

use sea_orm::DatabaseConnection;

use crate::auth::Authn;
use crate::cache::{CacheHandle, LockHandle, build_cache, build_lock, CacheError};
use crate::config::{self, ConfigHandle, Environment, LoadOptions, Settings};
use crate::db::pool::connect;
use crate::error::{AppError, AppResult};
use crate::observability::logging::LogGuard;
use crate::queue::{self, QueueHandle};

/// 框架上下文（应用 AppState 内嵌）。各字段为 `Option` 的：对应能力未配置时，
/// 相关中间件 / 提取器在使用处返回明确的错误或跳过。
pub struct CoreState {
    pub environment: Environment,
    /// 热切换配置：`state.config.load()` 取当前生效快照（读取零锁）
    pub config: ConfigHandle<Settings>,
    /// db 连接池（`[database].url` 为空时 None）
    pub db: Option<DatabaseConnection>,
    pub cache: CacheHandle,
    /// 与 cache 后端配对的锁（memory 进程内 / redis 分布式）
    pub lock: LockHandle,
    pub queue: QueueHandle,
    /// 认证链（`[auth].mode` 未启用任何方式时 None，全匿名）
    pub auth: Option<Arc<dyn Authn>>,
    /// 会话管理器（feature = "session" 且 mode 含 session）：登录/登出用
    #[cfg(feature = "session")]
    pub sessions: Option<Arc<crate::auth::session::SessionManager>>,
    /// JWT 签发器（feature = "jwt" 且 secret 非空）：登录签发 token 用
    #[cfg(feature = "jwt")]
    pub jwt: Option<crate::auth::jwt::Jwt>,
    /// OAuth2 provider 注册表（feature = "oauth2"）
    #[cfg(feature = "oauth2")]
    pub oauth2: Option<Arc<crate::auth::oauth2::OAuth2Registry>>,
    /// Casbin 强制器（feature = "casbin" 且 `[authz].enabled`）
    #[cfg(feature = "casbin")]
    pub authz: Option<Arc<crate::authz::Enforcer>>,
    /// 实时通信 hub（feature = "ws" / "sse"）
    #[cfg(any(feature = "ws", feature = "sse"))]
    pub hub: Arc<crate::realtime::hub::Hub>,
    /// 应用自定义健康探针（/ready 聚合；内置 db/cache/queue 探测无需注册）
    pub health_checks: std::sync::RwLock<Vec<Arc<dyn crate::observability::health::HealthCheck>>>,
    /// 日志后端保活（otel exporter / rotate-rs writer；Drop 时 flush）
    _log_guard: Option<LogGuard>,
}

impl Clone for CoreState {
    fn clone(&self) -> Self {
        Self {
            environment: self.environment,
            config: self.config.clone(),
            db: self.db.clone(),
            cache: self.cache.clone(),
            lock: self.lock.clone(),
            queue: self.queue.clone(),
            auth: self.auth.clone(),
            #[cfg(feature = "session")]
            sessions: self.sessions.clone(),
            #[cfg(feature = "jwt")]
            jwt: self.jwt.clone(),
            #[cfg(feature = "oauth2")]
            oauth2: self.oauth2.clone(),
            #[cfg(feature = "casbin")]
            authz: self.authz.clone(),
            #[cfg(any(feature = "ws", feature = "sse"))]
            hub: self.hub.clone(),
            health_checks: std::sync::RwLock::new(
                self.health_checks
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
            _log_guard: None, // 日志是进程级资源，克隆态不重复保活
        }
    }
}

impl std::fmt::Debug for CoreState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreState")
            .field("environment", &self.environment)
            .field("db", &self.db.is_some())
            .field("queue", &self.queue.name())
            .field("auth", &self.auth.as_ref().map(|a| a.name()))
            .finish_non_exhaustive()
    }
}

impl CoreState {
    /// 标准装配：`APP_ENV` → `config/` 目录加载配置 → 日志 → 数据库 → 缓存
    /// → 队列 → 认证 →（可选）热更新 watcher。任一步失败 fail-fast 直接退出。
    pub async fn bootstrap() -> AppResult<Self> {
        Self::bootstrap_in("config", Environment::from_env()).await
    }

    /// 指定配置目录与环境装配（测试与多实例进程用）
    pub async fn bootstrap_in(dir: &str, environment: Environment) -> AppResult<Self> {
        #[cfg_attr(not(feature = "config-remote"), allow(unused_mut))]
        let mut options = LoadOptions::new(environment).dir(dir);
        // 配置中心在 async 上下文预取（带超时+重试）；同步构建链只消费文本。
        // 此前在 build_config 里 Handle::block_on 桥接，async 上下文必 panic。
        #[cfg(feature = "config-remote")]
        if let Some(url) = &options.remote_url {
            let text = crate::config::source::fetch_remote_async(url).await.map_err(|e| {
                AppError::internal(format!("config center {url} unavailable: {e}"))
            })?;
            options.remote_text = Some(text);
        }
        let settings: Settings = config::load(&options)?;
        let state = Self::from_settings(settings, environment).await?;

        // 热更新（feature = "watch"）：文件监听 → 重载校验 → 原子替换 → 通知订阅者
        #[cfg(feature = "watch")]
        if state.config.load().server.watch.enabled {
            config::Watcher::new(options, state.config.clone(), state.config.load().server.watch.debounce_ms)
                .spawn();
            tracing::info!("config hot-reload watcher started");
        }
        Ok(state)
    }

    /// 程序化装配（测试 / TestApp / 内嵌场景）：跳过文件加载与 watcher。
    /// `settings.cache` 未显式选择后端时按默认 memory 装配。
    pub async fn from_settings(settings: Settings, environment: Environment) -> AppResult<Self> {
        // 日志最先初始化（后续步骤的日志才有输出；重复 init 安全忽略）
        let log_guard = crate::observability::logging::init(&settings);

        // 安全规则启动期校验（fail-fast）：ip_filter 规则解析失败绝不允许静默降级
        if settings.server.ip_filter.enabled {
            crate::middleware::ip_filter::IpFilter::from_rules(
                &settings.server.ip_filter.allow,
                &settings.server.ip_filter.deny,
            )
            .map_err(|e| AppError::internal(format!("invalid [server.ip_filter] rule: {e}")))?;
        }

        tracing::info!(
            environment = %environment,
            version = env!("CARGO_PKG_VERSION"),
            "core-rs bootstrapping"
        );

        // 数据库（fail-fast：连接失败直接退出）
        let db = if settings.database.enabled() {
            Some(connect(&settings.database).await.map_err(|e| {
                AppError::internal(format!("database connect failed: {e}"))
            })?)
        } else {
            None
        };

        // 缓存与锁（同一后端配对）
        let cache = build_cache(&settings.cache).map_err(cache_bootstrap_err)?;
        let lock = build_lock(&settings.cache).map_err(cache_bootstrap_err)?;

        // 队列
        let queue = queue::build(&settings.queue).await?;

        // 认证链
        let auth = crate::auth::build(&settings.auth, &cache)?;

        #[cfg(feature = "session")]
        let sessions = if settings.auth.modes().iter().any(|m| m == "session")
            && cfg!(feature = "session")
        {
            Some(Arc::new(crate::auth::session::SessionManager::new(
                cache.clone(),
                &settings.auth.session,
            )))
        } else {
            None
        };

        #[cfg(feature = "jwt")]
        let jwt = if !settings.auth.jwt.secret.is_empty() {
            Some(crate::auth::jwt::Jwt::new(&settings.auth.jwt)?)
        } else {
            None
        };

        #[cfg(feature = "oauth2")]
        let oauth2 = crate::auth::oauth2::OAuth2Registry::build(&settings.auth.oauth2)?
            .map(Arc::new);

        // Casbin（db 策略源需要连接池，先建连接再装配）
        #[cfg(feature = "casbin")]
        let authz = if settings.authz.enabled && settings.authz.auto_load {
            let mut adapter =
                crate::authz::adapter::DbOrFileAdapter::from_settings(&settings.authz).await?;
            if settings.authz.source == "db" {
                if let Some(db) = &db {
                    adapter.set_db(db.clone());
                }
            }
            // 把已注入连接的 adapter 传进去：Enforcer::new 会立即 load_policy，
            // 若在此处重新 from_settings，db 源会拿到 db=None 的空 adapter 而启动失败
            Some(Arc::new(
                crate::authz::Enforcer::build_with(&settings.authz, adapter).await?,
            ))
        } else {
            None
        };

        #[cfg(any(feature = "ws", feature = "sse"))]
        let hub = {
            let hub = Arc::new(crate::realtime::hub::Hub::new(&settings.realtime));
            // 跨实例转发（v1 经 Redis Pub/Sub）：要求 cache-redis + cache.backend = redis
            #[cfg(feature = "cache-redis")]
            if settings.realtime.enabled && settings.realtime.forward == "queue" {
                if settings.cache.backend == "redis" && settings.cache.redis.enabled() {
                    match crate::realtime::forward::Forwarder::start(
                        hub.clone(),
                        settings.realtime.forward_topic.clone(),
                        &settings.cache.redis.url,
                    )
                    .await
                    {
                        Ok(forwarder) => {
                            hub.set_forwarder(forwarder);
                            tracing::info!("realtime cross-instance forwarding enabled (Redis Pub/Sub)");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "realtime cross-instance forwarding disabled")
                        }
                    }
                } else {
                    tracing::warn!(
                        "realtime.forward = \"queue\" requires cache.backend = \"redis\" (Redis Pub/Sub), forwarding disabled"
                    );
                }
            }
            hub
        };

        tracing::info!(
            db = db.is_some(),
            cache_backend = settings.cache.backend.as_str(),
            queue_backend = queue.name(),
            "core-rs bootstrap complete"
        );

        Ok(Self {
            environment,
            config: ConfigHandle::new(settings),
            db,
            cache,
            lock,
            queue,
            auth,
            #[cfg(feature = "session")]
            sessions,
            #[cfg(feature = "jwt")]
            jwt,
            #[cfg(feature = "oauth2")]
            oauth2,
            #[cfg(feature = "casbin")]
            authz,
            #[cfg(any(feature = "ws", feature = "sse"))]
            hub,
            health_checks: std::sync::RwLock::new(Vec::new()),
            _log_guard: Some(log_guard),
        })
    }

    /// 注册应用自定义健康探针（/ready 聚合）
    pub fn register_health_check(&self, check: Arc<dyn crate::observability::health::HealthCheck>) {
        self.health_checks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(check);
    }
}

fn cache_bootstrap_err(e: CacheError) -> AppError {
    AppError::internal(format!("cache bootstrap failed: {e}"))
}
