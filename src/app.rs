//! App 构建器（文档 三·2）：装配 配置 → 日志 → 数据库 → 缓存 → 队列 →
//! 路由 → 服务，任一步失败 fail-fast 直接退出。
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
//!     .mount(Router::new())             // 用户端路由树（默认模式：框架自动装配完整推荐栈）
//!     .mount(Router::new())             // 管理端路由树
//!     .serve()                          // 优雅停机：监听 Ctrl-C / SIGTERM
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! **裸骨架模式**（`.bare()`）：框架只挂必需件（`Extension(CoreState)` /
//! panic 捕获 / request_id），中间件改由业务层按路由树自选：
//!
//! ```rust,ignore
//! # async fn demo_bare() -> anyhow::Result<()> {
//! # use core_rs::prelude::*;
//! # use core_rs::middleware::{auth, stack};
//! # #[derive(Clone)] struct AppState { core: CoreState }
//! # impl From<CoreState> for AppState { fn from(core: CoreState) -> Self { Self { core } } }
//! # let s = core_rs::config::Settings::default();
//! let admin = stack::common(                      // 公共无状态层打包收尾
//!     Router::new()                               // 由内向外挂状态件
//!         .layer(auth::require_identity_layer())  // 登录态兜底（无 Identity 401）
//!         .layer(auth::layer()),
//!     &s.server,
//! );
//! App::<AppState>::bootstrap()?
//!     .bare()
//!     .mount(admin)
//!     .serve()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! 框架负责生命周期，应用只提供两样东西：`AppState`（内嵌 `CoreState`）和
//! 若干棵路由树；`/health`、`/ready`、`/metrics` 由框架自动挂载。

use std::future::Future; // 引入 Future trait，用于约束异步消费任务的返回类型
use std::sync::Arc; // 引入原子引用计数指针，用于把闭包包成可跨线程共享的回调

use axum::Router; // 引入 axum 路由树类型，mount 的入参

use crate::config::{ConfigHandle, OnChange, Settings}; // 引入配置句柄、变更回调类型与全局设置
use crate::error::AppResult; // 引入框架统一结果类型，装配各步骤返回它
use crate::queue::worker::{Handler, Worker, WorkerRunner}; // 引入队列消费者句柄、worker 构建器与运行器
use crate::state::CoreState; // 引入框架核心状态，App 内部持有它
use crate::traits::{ // 引入 App 状态所需的各能力 trait，作为 impl 泛型上界
    HasCache, HasConfig, HasDb, HasHealthChecks, HasQueue, // 缓存/配置/数据库/健康探针/队列能力
};

/// 应用状态构造：`AppState` 内嵌 `CoreState` 时一行 `impl From<CoreState>` 即得
pub trait FromCore { // 定义「能从 CoreState 构造自身」的 trait，解耦框架与具体 AppState
    fn from_core(core: CoreState) -> Self; // 由核心状态构造应用状态
}

impl<T: From<CoreState>> FromCore for T { // 为所有实现了 From<CoreState> 的类型自动提供 FromCore
    fn from_core(core: CoreState) -> Self { // 实现 from_core 方法
        T::from(core) // 直接转发给标准库的 From::from
    }
}

/// App 构建器（链式；`serve` 消耗自身并阻塞运行）
pub struct App<S> { // 泛型参数 S 为应用状态类型（内嵌 CoreState）
    state: S, // 应用状态实例，供中间件 from_fn_with_state 使用
    core: CoreState, // 框架核心状态，供框架内部各步骤使用
    routers: Vec<Router<S>>, // 应用路由树集合（mount 挂载）
    /// 裸骨架模式（`bare()`）：serve 只挂框架必需件，中间件由应用按树自选
    bare: bool, // 是否裸骨架装配
    consumers: Vec<(String, Handler)>, // 待注册的队列消费任务（topic 与处理函数）
    #[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面字段
    jobs: Vec<crate::task::Job>, // 待注册的定时任务集合
    #[cfg(feature = "tls")] // 仅在开启 tls feature 时编译下面字段
    cert_provider: Option<Arc<dyn crate::tls::CertProvider>>, // 业务注册的证书来源
}

impl<S> App<S> // 为 App<S> 实现装配方法
where // 以下是 S 必须满足的 trait 上界
    S: Clone // 状态需可克隆（中间件按请求克隆状态）
        + Send // 可跨线程发送
        + Sync // 可跨线程共享引用
        + 'static // 不含非静态借用
        + FromCore // 能从 CoreState 构造
        + HasDb // 能提供数据库连接
        + HasCache // 能提供缓存与锁
        + HasQueue // 能提供队列
        + HasConfig // 能提供配置句柄
        + HasHealthChecks // 能提供健康探针列表
        + crate::traits::HasAuth, // 能提供认证器
{
    /// 标准装配：`APP_ENV` → `config/` 多环境配置（含热更新 watcher）→
    /// tracing → 连接池 → 缓存 → 队列。
    pub async fn bootstrap() -> AppResult<Self> { // 默认目录 config 与 APP_ENV 环境装配
        let core = CoreState::bootstrap().await?; // 先按标准流程构建核心状态
        Ok(Self::from_core(core)) // 再由核心状态构造 App
    }

    /// 指定配置目录与环境装配
    pub async fn bootstrap_in(dir: &str, environment: crate::config::Environment) -> AppResult<Self> { // 指定配置目录与环境装配
        let core = CoreState::bootstrap_in(dir, environment).await?; // 用指定目录与环境构建核心状态
        Ok(Self::from_core(core)) // 再由核心状态构造 App
    }

    fn from_core(core: CoreState) -> Self { // 由核心状态构造 App（内部使用）
        let state = S::from_core(core.clone()); // 克隆核心状态并构造应用状态
        Self { // 组装 App 各字段
            state, // 应用状态
            core, // 核心状态
            routers: Vec::new(), // 路由树初始为空
            bare: false, // 默认完整推荐栈装配
            consumers: Vec::new(), // 消费任务初始为空
            #[cfg(feature = "scheduler")] // 仅在开启 scheduler 时初始化 jobs
            jobs: Vec::new(), // 定时任务初始为空
            #[cfg(feature = "tls")] // 仅在开启 tls 时初始化
            cert_provider: None, // 证书来源初始未注册
        }
    }

    /// 挂载一棵路由树（可多次调用叠加；health/ready/metrics 由框架自动挂）。
    ///
    /// **框架只做合并，不做鉴权分类**：本方法不会给路由套任何鉴权层。
    /// 「哪些接口要登录」由业务决定——需要登录态的子树请在挂载前自行套层：
    ///
    /// ```rust,ignore
    /// use core_rs::middleware::auth;
    /// App::<AppState>::bootstrap().await?
    ///     .mount(api::public_routes())                                      // 公开：不套层
    ///     .mount(api::admin_routes().layer(auth::require_identity_layer())) // 受保护：无 Identity 401
    ///     .serve().await?;
    /// ```
    ///
    /// 层序：默认模式下框架已在**最外圈**挂 `auth::layer()`（注入 Identity），
    /// 故受保护子树只需挂**内层** `require_identity_layer()`。裸骨架模式
    /// （[`App::bare`]）下没有全局 auth，须自己按「内 → 外」挂两层。
    pub fn mount(mut self, router: Router<S>) -> Self { // 挂载路由树（只合并，不套鉴权层）
        self.routers.push(router); // 追加到路由集合
        self // 返回自身以支持链式调用
    }

    /// 裸骨架装配：`serve` 只挂三件**框架必需件**——`Extension(CoreState)`
    /// （最外圈，供全部状态件与路由级 `required()` 读取）、panic 捕获、
    /// request_id（locale/trace/access_log 的依赖）；其余中间件由应用在各自
    /// 路由树上自选（各中间件模块的 `layer()` 构造器 + [`middleware::stack`]
    /// 预设栈，见 middleware/mod.rs 头部文档）。
    ///
    /// [`App::mount`] 的语义不随模式变化：两种模式下都只做合并、不套鉴权层。
    pub fn bare(mut self) -> Self { // 切换为裸骨架装配
        self.bare = true; // 置位裸模式标记
        self // 返回自身以支持链式调用
    }

    /// 注册配置热更新回调（feature = "watch"）：重载成功后依次收到新配置快照
    pub fn on_config_change(self, f: impl Fn(&Settings) + Send + Sync + 'static) -> Self { // 注册配置变更回调
        let cb: OnChange<Settings> = Arc::new(f); // 把回调包进 Arc 以满足订阅接口要求
        self.core.config.subscribe(cb); // 向配置句柄订阅该回调
        self // 返回自身以支持链式调用
    }

    /// 配置句柄（应用需要在 serve 前读取 / 订阅配置时用）
    pub fn config(&self) -> &ConfigHandle<Settings> { // 暴露配置句柄供应用读取/订阅
        &self.core.config // 借用核心状态中的配置句柄
    }

    /// 注册队列消费任务（topic + handler）：装配时先全部订阅再启动消费。
    pub fn consumer<F, Fut>(mut self, topic: impl Into<String>, f: F) -> Self // 注册一个队列消费任务
    where // 以下是处理函数与其返回 Future 的约束
        F: Fn(crate::queue::Message) -> Fut + Send + Sync + 'static, // 处理函数：接收消息、返回 Future，且可跨线程
        Fut: Future<Output = Result<(), crate::queue::QueueError>> + Send + 'static, // 处理 Future：可 Send 且能静态存活
    {
        self.consumers // 向消费任务集合追加
            .push((topic.into(), Arc::new(move |msg| Box::pin(f(msg))))); // 把 topic 与装箱后的异步处理函数存入
        self // 返回自身以支持链式调用
    }

    /// 注册定时任务（feature = "scheduler"）：实际是否运行由 `[task]` 配置决定
    #[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面方法
    pub fn task(mut self, job: crate::task::Job) -> Self { // 注册一个定时任务
        self.jobs.push(job); // 追加到定时任务集合
        self // 返回自身以支持链式调用
    }

    /// 注册服务端 TLS 的证书来源（feature = "tls"）。
    ///
    /// 证书的保存 / 来源 / 轮换全在业务层：业务实现 [`crate::tls::CertProvider`]，
    /// 在 `serve()` 前注册。当 `[server.tls].enabled = true` 时必须注册，否则启动 fail-fast。
    ///
    /// ```rust,ignore
    /// App::<AppState>::bootstrap().await?
    ///     .cert_provider(Arc::new(MyCertProvider { /* 业务数据源 */ }))
    ///     .serve().await?;
    /// ```
    #[cfg(feature = "tls")] // 仅在开启 tls feature 时编译下面方法
    pub fn cert_provider(mut self, provider: Arc<dyn crate::tls::CertProvider>) -> Self { // 注册证书来源
        self.cert_provider = Some(provider); // 保存到字段
        self // 返回自身以支持链式调用
    }

    /// 执行完整装配并阻塞运行，直到收到退出信号（Ctrl+C / SIGTERM）。
    /// 停机顺序：停止接新请求（排空在途）→ 队列 worker 退出 → 调度器停止。
    #[cfg_attr(not(feature = "tls"), allow(unused_mut))] // 未开 tls 时 self 无需可变
    pub async fn serve(mut self) -> AppResult<()> { // 装配全部组件并阻塞运行直到停机
        #[cfg(feature = "scheduler")] // 仅在开启 scheduler 时提前取出 jobs
        let jobs = self.jobs; // 取出定时任务集合（后续 start_from_config 消费）

        // 1. 队列 worker（先注册后启动）
        let worker_runner: Option<WorkerRunner> = if !self.consumers.is_empty() { // 仅当有消费任务时才构建 worker
            let queue_settings = self.core.config.load().queue.clone(); // 读取当前队列配置快照
            let mut worker = Worker::new(self.core.queue.clone()) // 基于队列句柄创建 worker
                .concurrency(queue_settings.concurrency) // 设置并发数
                .max_attempts(queue_settings.max_attempts) // 设置最大重试次数
                .retry_backoff_ms(queue_settings.retry_backoff_ms); // 设置重试退避毫秒
            if !queue_settings.dead_letter_topic.is_empty() { // 若配置了死信 topic
                worker = worker.dead_letter_topic(queue_settings.dead_letter_topic); // 设置死信投递目标
            }
            for (topic, handler) in self.consumers { // 遍历全部消费任务
                worker = worker.consumer(topic, move |msg| handler(msg)); // 为每个 topic 注册处理函数
            }
            Some(worker.start().await?) // 启动 worker 并保留运行器句柄以便停机
        } else { // 无消费任务
            None // 不启动 worker
        };

        // 2. 定时任务调度器
        #[cfg(feature = "scheduler")] // 仅在开启 scheduler 时启动调度器
        let scheduler = crate::task::start_from_config(jobs, &self.state).await?; // 按配置启动定时任务调度器

        // 3. 路由装配：health/ready（+metrics）自动挂载，应用路由树随后合并。
        //    框架只做合并，不套任何鉴权层——「哪些接口要登录」由业务在挂载前
        //    自行套层（见 App::mount 文档）。
        let settings = self.core.config.load().clone(); // 取当前配置快照（后续多次读取）
        let server = &settings.server; // 引用其中的 server 配置段

        // TLS 装配（feature = "tls"）：仅在 [server.tls].enabled 时启用。
        // 证书由业务 CertProvider 提供；框架不持有证书、不定义存储约定、不内置来源。
        #[cfg(feature = "tls")] // 仅在开启 tls feature 时编译
        let tls_state: Option<Arc<crate::tls::TlsState>> = if settings.server.tls.enabled { // 配置启用了 TLS
            let tls = &settings.server.tls; // 引用 TLS 配置段
            let provider = self.cert_provider.clone().ok_or_else(|| { // 取业务注册的证书来源
                crate::error::AppError::internal( // 未注册则 fail-fast
                    "[server.tls].enabled = true 但未注册证书来源（App::cert_provider）", // 明确提示
                )
            })?;
            let state = Arc::new(crate::tls::TlsState::new(provider)); // 构造 TLS 状态
            state // 首次装载：失败 fail-fast（没有证书无法对外服务）
                .reload()
                .await
                .map_err(|e| crate::error::AppError::internal(format!("tls initial load failed: {e}")))?;
            self.core.tls = Some(state.clone()); // 注入 CoreState（供业务 core.tls.reload()）
            if tls.monitor_expiry { // 到期监控（TLS 开启即生效，与目录模式无关）
                self.core.register_health_check(Arc::new( // 注册到期探针到 /ready
                    crate::tls::reload::ExpiryProbe::new(state.clone()),
                ));
            }
            // 重新派生应用状态：让 handler 通过 core.tls 与探针看到最新装配
            self.state = S::from_core(self.core.clone()); // 用更新后的核心状态重建
            if tls.refresh_interval_secs > 0 { // 轮询刷新（默认 60s，0 = 关）
                crate::tls::reload::spawn_poller(state.clone(), tls.refresh_interval_secs); // 启动轮询任务
            }
            if !tls.dir.trim().is_empty() { // 目录模式：配了 dir 才启用目录监听
                crate::tls::reload::spawn_dir_watcher(&tls.dir, state.clone(), tls.debounce_ms); // 启动目录监听
            }
            tracing::info!( // 记录 TLS 已启用
                min_version = %tls.min_version, // 最低协议版本
                dir_mode = !tls.dir.trim().is_empty(), // 是否目录模式
                "core-rs TLS enabled" // 提示
            );
            Some(state) // 返回 TLS 状态
        } else { // 未启用 TLS
            None // 无 TLS 状态（走纯 HTTP 路径）
        };

        let mut router: Router<S> = crate::observability::health::routes::<S>(); // 以框架自带的 health/ready 路由为基座
        for r in self.routers { // 遍历应用路由树
            router = router.merge(r); // 依次合并进基座路由
        }

        #[cfg(feature = "metrics")] // 仅在开启 metrics feature 时装配指标端点
        let metrics_handle = { // 计算指标句柄（可能为 None）
            // 默认关闭：全量内部指标（路由/耗时/未匹配路径）不应无门控地公网暴露；
            // 需要时在配置里显式 [server.metrics] enabled = true，并自行置于内网/加访问控制
            if settings.server.metrics.enabled { // 若配置显式开启指标
                // 进程内只能安装一次 recorder（测试多 App 场景降级为不暴露 /metrics）
                let handle = crate::observability::metrics::init(); // 尝试初始化全局 recorder
                if let Some(h) = &handle { // 若初始化成功（本次为首个安装者）
                    let render = h.clone(); // 克隆句柄供 handler 闭包捕获
                    router = router.route( // 注册 /metrics 路由
                        "/metrics", // 指标端点路径
                        axum::routing::get(move || { // GET 处理器
                            let render = render.clone(); // 每次请求克隆句柄
                            async move { render.render() } // 异步渲染指标文本
                        }),
                    );
                    tracing::info!("metrics endpoint served at /metrics"); // 记录指标端点已启用
                }
                handle // 返回句柄
            } else { // 未开启指标
                None // 返回 None，后续不加 track 层
            }
        };

        // 4. 中间件装配（请求流由外到内，完整推荐顺序见 middleware/mod.rs）：
        //    - 默认模式：状态件（idempotency → auth → csrf → rate_limit →
        //      ip_filter，依赖最外圈 Extension(CoreState)）→ metrics::track
        //      → 公共无状态层（web/router.rs::assemble_base，panic/request_id
        //      在其中）→ Extension(CoreState) 收到最外圈
        //    - 裸骨架模式：应用在路由树上自选（middleware/stack.rs、各中间件
        //      layer()），框架只补 request_id 与 panic 两件必需件
        //    两种模式最后都把 Extension(CoreState) 挂在最外圈：所有内层中间件
        //    （含应用自挂的）与路由级 required() 由此读取 CoreState；缺扩展的
        //    中间件请求 500 fail-closed（说明装配违背了必需件约定）
        let router = if self.bare { // 裸骨架模式：只挂必需件
            #[cfg(feature = "metrics")] // 仅在开启 metrics feature 时叠加 track 层
            let router = if metrics_handle.is_some() { // 仅当指标端点已启用时
                router.layer(axum::middleware::from_fn(crate::observability::metrics::track)) // 叠加请求计数中间件
            } else { // 否则
                router // 原样返回
            };
            #[cfg(not(feature = "metrics"))] // 未开启 metrics 时不叠加
            let router = router; // 保持不变
            let router = router.layer(crate::middleware::request_id::layer()); // 必需件：双 id（locale/trace/access_log 的依赖）
            let router = crate::middleware::panic::layer(router); // 必需件：panic 捕获（统一 500）
            router.layer(axum::Extension(self.core.clone())) // 必需件：CoreState 注入（最外圈）
        } else { // 默认模式：完整推荐栈
            // 状态件自内向外挂（CoreState 经最外圈扩展读取）
            let router = router.layer(crate::middleware::idempotency::layer()); // 幂等层在 auth 内侧：需要 Identity 把回放缓存按用户隔离（防跨用户回放）
            let router = router.layer(crate::middleware::auth::layer()); // 认证中间件（注入 Identity）
            #[cfg(feature = "csrf")] // 仅在开启 csrf feature 时叠加
            let router = router.layer(crate::middleware::csrf::layer()); // CSRF 防护中间件
            #[cfg(feature = "rate-limit")] // 仅在开启 rate-limit feature 时叠加
            let router = router.layer(crate::middleware::rate_limit::layer()); // 限流中间件
            let router = router.layer(crate::middleware::ip_filter::layer()); // IP 过滤中间件

            // metrics 计数（MatchedPath 在路由匹配后注入；未匹配路由归一化为固定标签）
            #[cfg(feature = "metrics")] // 仅在开启 metrics feature 时叠加 track 层
            let router = if metrics_handle.is_some() { // 仅当指标端点已启用时
                router.layer(axum::middleware::from_fn(crate::observability::metrics::track)) // 叠加请求计数中间件
            } else { // 否则
                router // 原样返回
            };

            // 无状态公共层（web/router.rs 推荐位次；locale 在 request_id 之后协商）
            let router = crate::web::router::assemble_base( // 叠加一批无状态公共层
                router, // 现有路由
                server, // server 配置段
                settings.service_name(), // 服务名（用于日志/标签）
            );
            router.layer(axum::Extension(self.core.clone())) // CoreState 注入（最外圈，状态件与 required() 的依赖锚点）
        };

        let app = router // 把路由转为可服务对象
            .with_state(self.state.clone()) // 注入最终状态
            .into_make_service_with_connect_info::<std::net::SocketAddr>(); // 附带连接信息（提取客户端 IP 用）

        // 5. 监听与优雅停机
        let addr = format!("{}:{}", server.host, server.port); // 拼出监听地址 host:port
        let server_cfg_shutdown_timeout = server.shutdown_timeout_secs; // 取出停机等待超时秒数

        // ---- TLS 路径（feature = "tls" 且 [server.tls].enabled）----
        #[cfg(feature = "tls")] // 仅在开启 tls feature 时编译
        if let Some(tls_state) = tls_state { // 已装配 TLS 状态
            let tls = &settings.server.tls; // 引用 TLS 配置段
            let socket_addr: std::net::SocketAddr = addr // 解析监听地址
                .parse()
                .map_err(|e| crate::error::AppError::internal(format!("invalid listen addr `{addr}`: {e}")))?;
            let rustls_config = crate::tls::serve::build_rustls_config(&tls.min_version, tls_state.store()) // 构建 rustls 配置
                .map_err(|e| crate::error::AppError::internal(format!("tls config build failed: {e}")))?;
            tracing::info!("core-rs application listening on https://{addr}"); // 记录已开始监听（HTTPS）

            // 停机信号 → watch 广播（TLS 主服务与可选跳转服务共用同一信号）
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false); // 创建广播通道
            tokio::spawn(async move { // 后台等待停机信号
                shutdown_signal().await; // 等待 Ctrl-C / SIGTERM
                let _ = shutdown_tx.send(true); // 广播停机
            });

            // 可选：HTTP → HTTPS 308 跳转
            if tls.redirect_http { // 配置启用了跳转
                crate::tls::serve::spawn_http_redirect( // 启动跳转监听
                    server.host.clone(), // 监听地址
                    tls.http_port, // 明文端口
                    server.port, // 目标 HTTPS 端口
                    shutdown_rx.clone(), // 共用停机信号
                );
            }

            // 优雅停机：watch 触发 → axum-server Handle
            let handle: axum_server::Handle<std::net::SocketAddr> = axum_server::Handle::new(); // 创建停机句柄
            { // 后台把 watch 信号转成 Handle 优雅停机
                let handle = handle.clone(); // 克隆句柄
                let mut rx = shutdown_rx; // 取接收端（此后独占）
                let timeout = server_cfg_shutdown_timeout; // 停机等待秒数
                tokio::spawn(async move { // 后台任务
                    let _ = rx.changed().await; // 等待停机广播
                    handle.graceful_shutdown(if timeout > 0 { // 触发优雅停机
                        Some(std::time::Duration::from_secs(timeout)) // 限时等待在途请求
                    } else {
                        None // 不限时（一直等在途请求）
                    });
                });
            }

            let result = axum_server::tls_rustls::bind_rustls(socket_addr, rustls_config) // 绑定并启动 TLS 服务
                .handle(handle) // 挂接停机句柄
                .serve(app) // 提供服务（含连接信息）
                .await; // 阻塞运行直到停机

            // 后台件收尾（与 HTTP 路径一致）
            if let Some(worker) = worker_runner { // 若有队列 worker
                worker.shutdown().await; // 排空退出
            }
            #[cfg(feature = "scheduler")] // 仅在开启 scheduler 时收尾调度器
            if let Some(mut scheduler) = scheduler { // 若有调度器
                let _ = scheduler.shutdown().await; // 停止调度器
            }

            result.map_err(|e| crate::error::AppError::internal(format!("tls server error: {e}")))?; // 传播服务错误
            tracing::info!("core-rs application stopped"); // 记录应用已停止
            return Ok(()); // TLS 路径结束
        }

        let listener = tokio::net::TcpListener::bind(addr.as_str()).await?; // 绑定并监听 TCP 端口
        tracing::info!("core-rs application listening on http://{addr}"); // 记录已开始监听

        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()); // 启动服务并挂接优雅停机信号
        let result = if server_cfg_shutdown_timeout > 0 { // 若配置了停机超时
            // 停机等待超时后强制退出，避免在途请求卡死阻塞滚动发布
            match tokio::time::timeout(std::time::Duration::from_secs(server_cfg_shutdown_timeout), server).await // 带超时地等待服务退出
            {
                Ok(result) => result, // 正常退出，取结果
                Err(_) => { // 超时
                    tracing::warn!( // 告警日志
                        secs = server_cfg_shutdown_timeout, // 记录超时秒数
                        "graceful shutdown timed out, forcing exit" // 提示强制退出
                    );
                    // 超时路径同样要收尾后台件：worker 排空退出、调度器停止，
                    // 否则 memory 队列未 ack 消息直接丢失、在跑的 job 被砍断
                    if let Some(worker) = worker_runner { // 若有队列 worker
                        worker.shutdown().await; // 让其排空并退出
                    }
                    #[cfg(feature = "scheduler")] // 仅在开启 scheduler 时收尾调度器
                    if let Some(mut scheduler) = scheduler { // 若有调度器
                        let _ = scheduler.shutdown().await; // 停止调度器
                    }
                    return Ok(()); // 强制退出，返回成功
                }
            }
        } else { // 未配置停机超时
            server.await // 直接等待服务自然退出
        };

        result?; // 传播服务运行期错误

        // 6. 后台件收尾
        if let Some(worker) = worker_runner { // 若有队列 worker
            worker.shutdown().await; // 正常路径下同样排空退出
        }
        #[cfg(feature = "scheduler")] // 仅在开启 scheduler 时收尾调度器
        if let Some(mut scheduler) = scheduler { // 若有调度器
            let _ = scheduler.shutdown().await; // 停止调度器
        }

        tracing::info!("core-rs application stopped"); // 记录应用已停止
        Ok(()) // 正常返回
    }
}

/// Ctrl+C / SIGTERM 任一触发即通知 axum 进入优雅停机
async fn shutdown_signal() { // 组合 Ctrl+C 与 SIGTERM 的停机信号
    let ctrl_c = async { // 定义等待 Ctrl+C 的异步块
        let _ = tokio::signal::ctrl_c().await; // 等待 Ctrl+C（错误忽略）
    };

    #[cfg(unix)] // 仅在 Unix 平台编译 SIGTERM 分支
    let terminate = async { // 定义等待 SIGTERM 的异步块
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) { // 注册 SIGTERM 处理器
            Ok(mut sig) => { // 注册成功
                sig.recv().await; // 等待信号到达
            }
            Err(e) => { // 注册失败
                tracing::warn!(error = %e, "cannot install SIGTERM handler"); // 告警无法安装处理器
                std::future::pending::<()>().await; // 永远挂起，避免误触发停机
            }
        }
    };

    #[cfg(not(unix))] // 非 Unix 平台
    let terminate = std::future::pending::<()>(); // 用永不就绪的 Future 占位（仅依赖 Ctrl+C）

    tokio::select! { // 任一信号就绪即返回
        _ = ctrl_c => {}, // Ctrl+C 分支
        _ = terminate => {}, // SIGTERM 分支
    }

    tracing::info!("shutdown signal received, draining in-flight requests"); // 记录收到停机信号并开始排空
}
