//! 框架入口 [`Application`]：装配 配置 → 日志/otel → DB 池 → Redis 池 → 迁移 →
//! 队列 → 路由 → swagger/metrics → 中间件 → 定时任务 → 配置热更新 → 优雅停机。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use tokio::net::TcpListener;

use crate::config::AppConfig;
use crate::error::AppResult;
use crate::state::AppState;
use crate::{cache, logging, orm, web};

/// 启动前置钩子的返回 Future（如建表、灌种子数据）
pub type SetupFuture = Pin<Box<dyn Future<Output = Result<(), crate::error::AppError>> + Send>>;

#[cfg(feature = "watch")]
use crate::config::hot_reload::{OnChange, Watcher};

#[cfg(feature = "queue")]
type QueueTask = (String, crate::queue::Handler);

/// 框架统一入口。用法：`Application::builder().routes(...).run().await`
pub struct Application;

impl Application {
    pub fn builder() -> ApplicationBuilder {
        ApplicationBuilder::default()
    }
}

#[derive(Default)]
pub struct ApplicationBuilder {
    config_path: Option<String>,
    profile: Option<String>,
    watch_override: Option<bool>,
    routes: Vec<Router<AppState>>,
    setup: Option<Box<dyn FnOnce(AppState) -> SetupFuture + Send>>,
    on_config_change: Vec<OnChange>,
    #[cfg(feature = "migration")]
    migrations: Option<Box<dyn FnOnce(sea_orm::DatabaseConnection) -> SetupFuture + Send>>,
    #[cfg(feature = "swagger")]
    openapi: Option<utoipa::openapi::OpenApi>,
    #[cfg(feature = "scheduler")]
    cron_jobs: Vec<crate::scheduler::CronJob>,
    #[cfg(feature = "queue")]
    queue_tasks: Vec<QueueTask>,
}

impl ApplicationBuilder {
    /// 指定配置文件路径，默认 `app.yml`（相对当前工作目录）
    pub fn config_file(mut self, path: impl Into<String>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// 指定 profile（叠加加载 `app-{profile}.yml`）；`CORE_PROFILE` 环境变量优先
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// 强制开/关配置热更新（feature = "watch"，默认开启）；
    /// 缺省以 `[watch].enabled` 配置为准
    pub fn watch(mut self, enabled: bool) -> Self {
        self.watch_override = Some(enabled);
        self
    }

    /// 注册一组路由，可多次调用叠加。路由状态类型固定为 [`AppState`]。
    pub fn routes(mut self, router: Router<AppState>) -> Self {
        self.routes.push(router);
        self
    }

    /// 注册启动前置钩子（拿到完整 AppState，可建表/灌数据），失败则中止启动。
    pub fn setup(mut self, f: impl FnOnce(AppState) -> SetupFuture + Send + 'static) -> Self {
        self.setup = Some(Box::new(f));
        self
    }

    /// 注册配置热更新回调（feature = "watch"）：配置文件变更重载成功后，
    /// 依次收到新配置快照。回调在监听线程同步执行，请保持轻量。
    #[cfg(feature = "watch")]
    pub fn on_config_change(
        mut self,
        f: impl Fn(&AppConfig) + Send + Sync + 'static,
    ) -> Self {
        self.on_config_change.push(Arc::new(f));
        self
    }

    /// 注册迁移器（feature = "migration"）：启动时自动执行所有未应用的迁移。
    #[cfg(feature = "migration")]
    pub fn migrations<M: sea_orm_migration::MigratorTrait + 'static>(
        mut self,
        _migrator: M,
    ) -> Self {
        self.migrations = Some(Box::new(|db| {
            Box::pin(async move { orm::migrate::up::<M>(&db).await })
        }));
        self
    }

    /// 注册 OpenAPI 文档（feature = "swagger"）：挂载 /swagger-ui 与 /api-docs/openapi.json
    #[cfg(feature = "swagger")]
    pub fn openapi(mut self, doc: utoipa::openapi::OpenApi) -> Self {
        self.openapi = Some(doc);
        self
    }

    /// 注册 cron 定时任务（feature = "scheduler"），schedule 为秒开头的 6/7 段 cron
    #[cfg(feature = "scheduler")]
    pub fn cron_job<F>(mut self, name: impl Into<String>, schedule: impl Into<String>, f: F) -> Self
    where
        F: Fn() -> crate::scheduler::CronFuture + Send + Sync + 'static,
    {
        self.cron_jobs
            .push(crate::scheduler::CronJob::new(name, schedule, f));
        self
    }

    /// 注册多实例互斥的 cron 定时任务（feature = "scheduler" + "dist-lock"，需 redis）：
    /// 按 job name 加分布式锁，抢不到锁的实例本轮跳过，避免多实例重复执行
    #[cfg(all(feature = "scheduler", feature = "dist-lock"))]
    pub fn cron_job_distributed<F>(
        mut self,
        name: impl Into<String>,
        schedule: impl Into<String>,
        f: F,
    ) -> Self
    where
        F: Fn() -> crate::scheduler::CronFuture + Send + Sync + 'static,
    {
        self.cron_jobs
            .push(crate::scheduler::CronJob::new(name, schedule, f).distributed());
        self
    }

    /// 注册队列消费任务（feature = "queue"）：topic + handler，装配时先全部
    /// 订阅再启动消费。handler 失败重试语义见 `queue` 模块文档。
    ///
    /// ```no_run
    /// # use core_rs::prelude::*;
    /// # async fn demo() -> Result<(), AppError> {
    /// Application::builder()
    ///     .queue_task("email.send", |msg| async move {
    ///         tracing::info!(payload = %msg.values, "sending email");
    ///         Ok(())
    ///     })
    ///     .run().await
    /// # }
    /// ```
    #[cfg(feature = "queue")]
    pub fn queue_task<F, Fut>(mut self, topic: impl Into<String>, f: F) -> Self
    where
        F: Fn(crate::queue::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), crate::queue::QueueError>> + Send + 'static,
    {
        self.queue_tasks.push((
            topic.into(),
            Arc::new(move |msg| Box::pin(f(msg))),
        ));
        self
    }

    /// 执行完整装配并阻塞运行，直到收到退出信号（Ctrl+C / SIGTERM）。
    pub async fn run(self) -> AppResult<()> {
        #[cfg(feature = "scheduler")]
        let cron_jobs = self.cron_jobs;
        #[cfg(feature = "queue")]
        let queue_tasks = self.queue_tasks;
        #[cfg(feature = "watch")]
        let on_change = self.on_config_change;

        let cfg = Arc::new(AppConfig::load_with_profile(
            self.config_path.as_deref(),
            self.profile.as_deref(),
        )?);
        // otel guard 需持有到进程结束（Drop 时 flush spans）
        let _otel_guard: logging::LogGuard = logging::init(&cfg);

        let db = if cfg.datasource.url.is_empty() {
            None
        } else {
            Some(orm::pool::connect(&cfg.datasource).await?)
        };
        let cache = cache::build(&cfg.cache, &cfg.redis)?;
        // 调度器在 with_state(state) 消耗 state 之后启动，提前留一份缓存句柄
        #[cfg(feature = "scheduler")]
        let scheduler_cache = cache.clone();

        #[cfg(feature = "queue")]
        let queue = crate::queue::build(&cfg.queue, &cfg.redis).await?;

        let state = AppState {
            config: Arc::new(ArcSwap::from(cfg.clone())),
            db,
            cache,
            #[cfg(feature = "queue")]
            queue: queue.clone(),
            #[cfg(feature = "jwt")]
            jwt: if cfg.jwt.secret.is_empty() {
                None
            } else {
                Some(crate::security::Jwt::new(&cfg.jwt)?)
            },
        };

        // 热更新监听用的配置句柄：with_state(state) 消耗 state 前留一份引用
        #[cfg(feature = "watch")]
        let config_watch_handle = state.config.clone();

        // 迁移先于 setup 钩子执行（钩子可能需要依赖迁移建好的表）
        #[cfg(feature = "migration")]
        if let Some(migrations) = self.migrations {
            match state.db.clone() {
                Some(db) => migrations(db).await?,
                None => {
                    return Err(crate::error::AppError::internal(
                        "migrations registered but datasource url is empty",
                    ))
                }
            }
        }

        if let Some(setup) = self.setup {
            setup(state.clone()).await?;
        }

        // 队列：先全部订阅再启动（redis 后端订阅即建组，启动与订阅之间的消息不丢）
        #[cfg(feature = "queue")]
        if let Some(q) = &queue {
            for (topic, handler) in &queue_tasks {
                q.subscribe(topic, handler.clone()).await?;
            }
            if !queue_tasks.is_empty() {
                q.clone().start().await?;
            }
        }

        let mut router: Router<AppState> = Router::new().merge(web::health::routes());
        for r in self.routes {
            router = router.merge(r);
        }

        #[cfg(feature = "swagger")]
        if let Some(doc) = self.openapi {
            router = router.merge(
                utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
                    .url("/api-docs/openapi.json", doc),
            );
            tracing::info!("swagger ui served at /swagger-ui");
        }

        #[cfg(feature = "metrics")]
        let _metrics = {
            let handle = crate::observe::metrics::init();
            router = router.layer(axum::middleware::from_fn(crate::observe::metrics::track));
            router = router.merge(crate::observe::metrics::routes(handle.clone()));
            Some(handle)
        };

        let app = web::middleware::apply(router.with_state(state), &cfg.server, cfg.service_name());

        #[cfg(feature = "upload")]
        let app = match &cfg.server.static_dir {
            Some(dir) => {
                tracing::info!(dir = %dir, "static files served at /static");
                app.nest_service("/static", tower_http::services::ServeDir::new(dir))
            }
            None => app,
        };

        #[cfg(feature = "scheduler")]
        let _scheduler = if cron_jobs.is_empty() {
            None
        } else {
            Some(crate::scheduler::start(cron_jobs, scheduler_cache.as_ref()).await?)
        };

        // 配置热更新（feature = "watch"）：监听线程重载成功后原子切换配置快照，
        // handler 里 `state.config.load()` 拿到的总是当前生效配置
        #[cfg(feature = "watch")]
        if self.watch_override.unwrap_or(cfg.watch.enabled) {
            let profile = std::env::var("CORE_PROFILE")
                .ok()
                .filter(|p| !p.is_empty())
                .or_else(|| self.profile.clone());
            Watcher::new(
                self.config_path.as_deref().unwrap_or("app.yml"),
                profile,
                config_watch_handle,
                on_change,
            )
            .spawn();
            tracing::info!("config hot-reload watcher started");
        }

        let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
        let listener = TcpListener::bind(addr.as_str()).await?;
        tracing::info!("core-rs application listening on http://{addr}");

        // into_make_service_with_connect_info：handler 可用 ConnectInfo<SocketAddr> 拿客户端地址
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal());
        if cfg.server.shutdown_timeout_secs > 0 {
            // 停机等待超时后强制退出，避免在途请求卡死阻塞滚动发布
            match tokio::time::timeout(
                std::time::Duration::from_secs(cfg.server.shutdown_timeout_secs),
                server,
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    tracing::warn!(
                        secs = cfg.server.shutdown_timeout_secs,
                        "graceful shutdown timed out, forcing exit"
                    )
                }
            }
        } else {
            server.await?;
        }

        tracing::info!("core-rs application stopped");
        Ok(())
    }
}

/// Ctrl+C / SIGTERM 任一触发即通知 axum 进入优雅停机
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, draining in-flight requests");
}
