//! App 构建器（文档 三·2）：装配 配置 → 日志 → 数据库 → 缓存 → 队列 →
//! （迁移）→ 路由 → 服务，任一步失败 fail-fast 直接退出。
//!
//! ```rust,ignore
//! // 示意（签名以实现为准，文档 三·2）；完整可编译示例见 examples/demo
//! # use core_rs::prelude::*;
//! # use axum::Router;
//! # #[derive(Clone)] struct AppState { core: CoreState }
//! # impl From<CoreState> for AppState {
//! #     fn from(core: CoreState) -> Self { Self { core } }
//! # }
//! # async fn demo() -> anyhow::Result<()> {
//! App::<AppState>::bootstrap()?         // APP_ENV → 多环境配置(含热更新) → tracing → 连接池 → 缓存 → 队列
//!     .mount(Router::new())             // 用户端路由树（挂哪些中间件由路由树自己决定）
//!     .mount(Router::new())             // 管理端路由树
//!     .serve()                          // 优雅停机：监听 Ctrl-C / SIGTERM
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! 框架负责生命周期，应用只提供两样东西：`AppState`（内嵌 `CoreState`）和
//! 若干棵路由树；`/health`、`/ready`、`/metrics` 由框架自动挂载。

use std::future::Future;
#[allow(unused_imports)]
use std::pin::Pin; // migration feature 下 MigrationFuture 使用
use std::sync::Arc;

use axum::Router;

use crate::config::{ConfigHandle, OnChange, Settings};
use crate::error::AppResult;
use crate::queue::worker::{Handler, Worker, WorkerRunner};
use crate::state::CoreState;
use crate::traits::{
    HasCache, HasConfig, HasDb, HasHealthChecks, HasQueue,
};

/// 应用状态构造：`AppState` 内嵌 `CoreState` 时一行 `impl From<CoreState>` 即得
pub trait FromCore {
    fn from_core(core: CoreState) -> Self;
}

impl<T: From<CoreState>> FromCore for T {
    fn from_core(core: CoreState) -> Self {
        T::from(core)
    }
}

/// 迁移执行闭包
#[cfg(feature = "migration")]
type MigrationFuture = Pin<Box<dyn Future<Output = AppResult<()>> + Send>>;
#[cfg(feature = "migration")]
type MigrationFn = Box<dyn FnOnce(sea_orm::DatabaseConnection) -> MigrationFuture + Send>;

/// App 构建器（链式；`serve` 消耗自身并阻塞运行）
pub struct App<S> {
    state: S,
    core: CoreState,
    routers: Vec<Router<S>>,
    consumers: Vec<(String, Handler)>,
    #[cfg(feature = "scheduler")]
    jobs: Vec<crate::task::Job>,
    #[cfg(feature = "migration")]
    migrations: Option<MigrationFn>,
}

impl<S> App<S>
where
    S: Clone
        + Send
        + Sync
        + 'static
        + FromCore
        + HasDb
        + HasCache
        + HasQueue
        + HasConfig
        + HasHealthChecks
        + crate::traits::HasAuth,
{
    /// 标准装配：`APP_ENV` → `config/` 多环境配置（含热更新 watcher）→
    /// tracing → 连接池 → 缓存 → 队列。
    pub async fn bootstrap() -> AppResult<Self> {
        let core = CoreState::bootstrap().await?;
        Ok(Self::from_core(core))
    }

    /// 指定配置目录与环境装配
    pub async fn bootstrap_in(dir: &str, environment: crate::config::Environment) -> AppResult<Self> {
        let core = CoreState::bootstrap_in(dir, environment).await?;
        Ok(Self::from_core(core))
    }

    fn from_core(core: CoreState) -> Self {
        let state = S::from_core(core.clone());
        Self {
            state,
            core,
            routers: Vec::new(),
            consumers: Vec::new(),
            #[cfg(feature = "scheduler")]
            jobs: Vec::new(),
            #[cfg(feature = "migration")]
            migrations: None,
        }
    }

    /// 挂载一棵路由树（可多次调用叠加；health/ready/metrics 由框架自动挂）
    pub fn mount(mut self, router: Router<S>) -> Self {
        self.routers.push(router);
        self
    }

    /// 注册配置热更新回调（feature = "watch"）：重载成功后依次收到新配置快照
    pub fn on_config_change(self, f: impl Fn(&Settings) + Send + Sync + 'static) -> Self {
        let cb: OnChange<Settings> = Arc::new(f);
        self.core.config.subscribe(cb);
        self
    }

    /// 配置句柄（应用需要在 serve 前读取 / 订阅配置时用）
    pub fn config(&self) -> &ConfigHandle<Settings> {
        &self.core.config
    }

    /// 注册队列消费任务（topic + handler）：装配时先全部订阅再启动消费。
    pub fn consumer<F, Fut>(mut self, topic: impl Into<String>, f: F) -> Self
    where
        F: Fn(crate::queue::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), crate::queue::QueueError>> + Send + 'static,
    {
        self.consumers
            .push((topic.into(), Arc::new(move |msg| Box::pin(f(msg)))));
        self
    }

    /// 注册定时任务（feature = "scheduler"）：实际是否运行由 `[task]` 配置决定
    #[cfg(feature = "scheduler")]
    pub fn task(mut self, job: crate::task::Job) -> Self {
        self.jobs.push(job);
        self
    }

    /// 注册迁移器（feature = "migration"）：serve 前自动执行所有未应用的迁移
    #[cfg(feature = "migration")]
    pub fn migrations<M: sea_orm_migration::MigratorTrait + 'static>(mut self, _migrator: M) -> Self {
        self.migrations = Some(Box::new(|db| {
            Box::pin(async move {
                crate::db::migrator::up::<M>(&db).await?;
                Ok(())
            })
        }));
        self
    }

    /// 执行完整装配并阻塞运行，直到收到退出信号（Ctrl+C / SIGTERM）。
    /// 停机顺序：停止接新请求（排空在途）→ 队列 worker 退出 → 调度器停止。
    pub async fn serve(self) -> AppResult<()> {
        #[cfg(feature = "scheduler")]
        let jobs = self.jobs;

        // 1. 迁移（应用侧 migrations 目录）
        #[cfg(feature = "migration")]
        if let Some(migrations) = self.migrations {
            match &self.core.db {
                Some(db) => migrations(db.clone()).await?,
                None => {
                    return Err(crate::error::AppError::internal(
                        "migrations registered but database url is empty",
                    ))
                }
            }
        }

        // 2. 队列 worker（先注册后启动）
        let worker_runner: Option<WorkerRunner> = if !self.consumers.is_empty() {
            let queue_settings = self.core.config.load().queue.clone();
            let mut worker = Worker::new(self.core.queue.clone())
                .concurrency(queue_settings.concurrency)
                .max_attempts(queue_settings.max_attempts)
                .retry_backoff_ms(queue_settings.retry_backoff_ms);
            if !queue_settings.dead_letter_topic.is_empty() {
                worker = worker.dead_letter_topic(queue_settings.dead_letter_topic);
            }
            for (topic, handler) in self.consumers {
                worker = worker.consumer(topic, move |msg| handler(msg));
            }
            Some(worker.start().await?)
        } else {
            None
        };

        // 3. 定时任务调度器
        #[cfg(feature = "scheduler")]
        let scheduler = crate::task::start_from_config(jobs, &self.state).await?;

        // 4. 路由装配：health/ready（+metrics）自动挂载，应用路由树随后合并
        let mut router: Router<S> = crate::observability::health::routes::<S>();
        for r in self.routers {
            router = router.merge(r);
        }

        let settings = self.core.config.load().clone();
        let server = &settings.server;

        #[cfg(feature = "metrics")]
        let metrics_handle = {
            // 默认关闭：全量内部指标（路由/耗时/未匹配路径）不应无门控地公网暴露；
            // 需要时在配置里显式 [server.metrics] enabled = true，并自行置于内网/加访问控制
            if settings.server.metrics.enabled {
                // 进程内只能安装一次 recorder（测试多 App 场景降级为不暴露 /metrics）
                let handle = crate::observability::metrics::init();
                if let Some(h) = &handle {
                    let render = h.clone();
                    router = router.route(
                        "/metrics",
                        axum::routing::get(move || {
                            let render = render.clone();
                            async move { render.render() }
                        }),
                    );
                    tracing::info!("metrics endpoint served at /metrics");
                }
                handle
            } else {
                None
            }
        };

        // 5. 中间件装配（自内向外挂，最终外→内顺序见 middleware/mod.rs）：
        //    ip_filter → rate_limit → csrf → auth → idempotency（状态件）
        //    → metrics → cors → compression → panic → request_id → locale
        //    → trace → access_log → security_headers → body_limit → timeout
        // CoreState 挂 extension：authz 强制层（required()，路由内层）经此拿强制器
        let router = router.layer(axum::Extension(self.core.clone()));
        // 幂等层在 auth 内侧：需要 Identity 把回放缓存按用户隔离（防跨用户回放）
        let router = router.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::idempotency::handle::<S>,
        ));
        let router = router.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::auth::handle::<S>,
        ));
        #[cfg(feature = "csrf")]
        let router = router.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::csrf::handle::<S>,
        ));
        #[cfg(feature = "rate-limit")]
        let router = router.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::rate_limit::handle::<S>,
        ));
        let router = router.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::ip_filter::handle::<S>,
        ));

        // metrics 计数（MatchedPath 在路由匹配后注入；未匹配路由归一化为固定标签）
        #[cfg(feature = "metrics")]
        let router = if metrics_handle.is_some() {
            router.layer(axum::middleware::from_fn(crate::observability::metrics::track))
        } else {
            router
        };

        // 无状态公共层（web/router.rs 推荐位次；locale 在 request_id 之后协商）
        let router = crate::web::router::assemble_base(
            router,
            self.state.clone(),
            server,
            settings.service_name(),
        );

        let app = router
            .with_state(self.state.clone())
            .into_make_service_with_connect_info::<std::net::SocketAddr>();

        // 6. 监听与优雅停机
        let addr = format!("{}:{}", server.host, server.port);
        let server_cfg_shutdown_timeout = server.shutdown_timeout_secs;
        let listener = tokio::net::TcpListener::bind(addr.as_str()).await?;
        tracing::info!("core-rs application listening on http://{addr}");

        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
        let result = if server_cfg_shutdown_timeout > 0 {
            // 停机等待超时后强制退出，避免在途请求卡死阻塞滚动发布
            match tokio::time::timeout(std::time::Duration::from_secs(server_cfg_shutdown_timeout), server).await
            {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        secs = server_cfg_shutdown_timeout,
                        "graceful shutdown timed out, forcing exit"
                    );
                    // 超时路径同样要收尾后台件：worker 排空退出、调度器停止，
                    // 否则 memory 队列未 ack 消息直接丢失、在跑的 job 被砍断
                    if let Some(worker) = worker_runner {
                        worker.shutdown().await;
                    }
                    #[cfg(feature = "scheduler")]
                    if let Some(mut scheduler) = scheduler {
                        let _ = scheduler.shutdown().await;
                    }
                    return Ok(());
                }
            }
        } else {
            server.await
        };

        result?;

        // 7. 后台件收尾
        if let Some(worker) = worker_runner {
            worker.shutdown().await;
        }
        #[cfg(feature = "scheduler")]
        if let Some(mut scheduler) = scheduler {
            let _ = scheduler.shutdown().await;
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
