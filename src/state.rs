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

use std::sync::Arc; // 引入原子引用计数指针，用于共享认证器/会话管理器等

use sea_orm::DatabaseConnection; // 引入 SeaORM 数据库连接类型

use crate::auth::Authn; // 引入认证器 trait，CoreState 以 trait 对象形式持有
use crate::cache::{CacheHandle, LockHandle, build_cache, build_lock, CacheError}; // 引入缓存/锁句柄与构建函数、错误类型
use crate::config::{self, ConfigHandle, Environment, LoadOptions, Settings}; // 引入配置加载相关类型与全局设置
use crate::db::pool::connect; // 引入数据库连接池构建函数
use crate::error::{AppError, AppResult}; // 引入框架统一错误与结果类型
use crate::observability::logging::LogGuard; // 引入日志后端保活句柄
use crate::queue::{self, QueueHandle}; // 引入队列模块与队列句柄

/// 框架上下文（应用 AppState 内嵌）。各字段为 `Option` 的：对应能力未配置时，
/// 相关中间件 / 提取器在使用处返回明确的错误或跳过。
pub struct CoreState { // 框架核心状态结构体
    pub environment: Environment, // 当前运行环境（dev/staging/prod）
    /// 热切换配置：`state.config.load()` 取当前生效快照（读取零锁）
    pub config: ConfigHandle<Settings>, // 配置句柄，支持热更新与零锁读取
    /// db 连接池（`[database].url` 为空时 None）
    pub db: Option<DatabaseConnection>, // 可选的数据库连接池
    pub cache: CacheHandle, // 缓存句柄（memory 进程内 / redis 分布式）
    /// 与 cache 后端配对的锁（memory 进程内 / redis 分布式）
    pub lock: LockHandle, // 与缓存后端配对的分布式/进程内锁
    pub queue: QueueHandle, // 队列句柄（发布/订阅消息）
    /// 认证链（`[auth].mode` 未启用任何方式时 None，全匿名）
    pub auth: Option<Arc<dyn Authn>>, // 可选认证器（session/jwt/oauth2 组合）
    /// 会话管理器（feature = "session" 且 mode 含 session）：登录/登出用
    #[cfg(feature = "session")] // 仅在开启 session feature 时编译下面字段
    pub sessions: Option<Arc<crate::auth::session::SessionManager>>, // 可选会话管理器
    /// JWT 签发器（feature = "jwt" 且 secret 非空）：登录签发 token 用
    #[cfg(feature = "jwt")] // 仅在开启 jwt feature 时编译下面字段
    pub jwt: Option<crate::auth::jwt::Jwt>, // 可选 JWT 签发/校验器
    /// OAuth2 provider 注册表（feature = "oauth2"）
    #[cfg(feature = "oauth2")] // 仅在开启 oauth2 feature 时编译下面字段
    pub oauth2: Option<Arc<crate::auth::oauth2::OAuth2Registry>>, // 可选 OAuth2 provider 注册表
    /// Casbin 强制器（feature = "casbin" 且 `[authz].enabled`）
    #[cfg(feature = "casbin")] // 仅在开启 casbin feature 时编译下面字段
    pub authz: Option<Arc<crate::authz::Enforcer>>, // 可选授权强制器
    /// 实时通信 hub（feature = "ws" / "sse"）
    #[cfg(any(feature = "ws", feature = "sse"))] // 开启 ws 或 sse 任一 feature 时编译下面字段
    pub hub: Arc<crate::realtime::hub::Hub>, // 实时通信中心
    /// 服务端 TLS 状态（feature = "tls" 且 `[server.tls].enabled`）：
    /// 证书仓库 + 业务来源；业务可调 `tls.reload()` 手动触发证书热更新
    #[cfg(feature = "tls")] // 仅在开启 tls feature 时编译下面字段
    pub tls: Option<Arc<crate::tls::TlsState>>, // 可选的 TLS 状态
    /// 应用自定义健康探针（/ready 聚合；内置 db/cache/queue 探测无需注册）
    pub health_checks: std::sync::RwLock<Vec<Arc<dyn crate::observability::health::HealthCheck>>>, // 读写锁保护的自定义健康探针列表
    /// 日志后端保活（otel exporter / rotate-rs writer；Drop 时 flush）
    _log_guard: Option<LogGuard>, // 日志保活句柄，Drop 时刷新并释放日志后端
}

impl Clone for CoreState { // 手动实现 Clone（部分字段需特殊处理）
    fn clone(&self) -> Self { // 克隆核心状态
        Self { // 逐字段克隆构造新实例
            environment: self.environment, // 环境枚举按值复制
            config: self.config.clone(), // 克隆配置句柄（内部共享）
            db: self.db.clone(), // 克隆连接池句柄（内部共享连接）
            cache: self.cache.clone(), // 克隆缓存句柄
            lock: self.lock.clone(), // 克隆锁句柄
            queue: self.queue.clone(), // 克隆队列句柄
            auth: self.auth.clone(), // 克隆认证器 Arc
            #[cfg(feature = "session")] // 仅在开启 session 时克隆
            sessions: self.sessions.clone(), // 克隆会话管理器 Arc
            #[cfg(feature = "jwt")] // 仅在开启 jwt 时克隆
            jwt: self.jwt.clone(), // 克隆 JWT 签发器
            #[cfg(feature = "oauth2")] // 仅在开启 oauth2 时克隆
            oauth2: self.oauth2.clone(), // 克隆 OAuth2 注册表 Arc
            #[cfg(feature = "casbin")] // 仅在开启 casbin 时克隆
            authz: self.authz.clone(), // 克隆授权强制器 Arc
            #[cfg(any(feature = "ws", feature = "sse"))] // 开启 ws 或 sse 时克隆
            hub: self.hub.clone(), // 克隆实时通信 Hub
            #[cfg(feature = "tls")] // 仅在开启 tls 时克隆
            tls: self.tls.clone(), // 克隆 TLS 状态 Arc
            health_checks: std::sync::RwLock::new( // 探针列表需深拷贝一份新的读写锁
                self.health_checks // 读取原列表
                    .read() // 获取读锁
                    .unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值
                    .clone(), // 克隆探针 Vec
            ),
            _log_guard: None, // 日志是进程级资源，克隆态不重复保活
        }
    }
}

impl std::fmt::Debug for CoreState { // 手动实现 Debug（避免打印不可 Debug 的字段）
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现格式化输出
        let mut d = f.debug_struct("CoreState"); // 以结构体形式开始
        d.field("environment", &self.environment) // 输出环境
            .field("db", &self.db.is_some()) // 仅输出是否已配置数据库
            .field("queue", &self.queue.name()) // 输出队列后端名
            .field("auth", &self.auth.as_ref().map(|a| a.name())); // 输出认证方式名（可选）
        #[cfg(feature = "tls")] // 仅在开启 tls 时输出该字段
        d.field("tls", &self.tls.is_some()); // 仅输出是否已启用 TLS
        d.finish_non_exhaustive() // 其余字段省略，标记为非穷尽
    }
}

impl CoreState { // 核心状态的装配与注册方法
    /// 标准装配：`APP_ENV` → `config/` 目录加载配置 → 日志 → 数据库 → 缓存
    /// → 队列 → 认证 →（可选）热更新 watcher。任一步失败 fail-fast 直接退出。
    pub async fn bootstrap() -> AppResult<Self> { // 标准装配入口
        Self::bootstrap_in("config", Environment::from_env()).await // 用默认目录与环境走 bootstrap_in
    }

    /// 指定配置目录与环境装配（测试与多实例进程用）
    pub async fn bootstrap_in(dir: &str, environment: Environment) -> AppResult<Self> { // 指定目录与环境装配
        #[cfg_attr(not(feature = "config-remote"), allow(unused_mut))] // 未开 config-remote 时允许 mut 未使用
        let mut options = LoadOptions::new(environment).dir(dir); // 构造加载选项（环境 + 目录）
        // 配置中心在 async 上下文预取（带超时+重试）；同步构建链只消费文本。
        // 此前在 build_config 里 Handle::block_on 桥接，async 上下文必 panic。
        #[cfg(feature = "config-remote")] // 仅在开启 config-remote 时预取远端配置
        if let Some(url) = &options.remote_url { // 若配置了配置中心地址
            let text = crate::config::source::fetch_remote_async(url).await.map_err(|e| { // 异步拉取远端配置文本
                AppError::internal(format!("config center {url} unavailable: {e}")) // 拉取失败转为内部错误
            })?;
            options.remote_text = Some(text); // 把远端文本填入选项供后续合并
        }
        let settings: Settings = config::load(&options)?; // 加载并合并出最终设置
        let state = Self::from_settings(settings, environment).await?; // 按设置装配核心状态

        // 热更新（feature = "watch"）：文件监听 → 重载校验 → 原子替换 → 通知订阅者
        #[cfg(feature = "watch")] // 仅在开启 watch 时启动热更新
        if state.config.load().server.watch.enabled { // 若配置启用了热更新
            config::Watcher::new(options, state.config.clone(), state.config.load().server.watch.debounce_ms) // 创建文件监听器
                .spawn(); // 启动监听后台任务
            tracing::info!("config hot-reload watcher started"); // 记录 watcher 已启动
        }
        Ok(state) // 返回装配好的核心状态
    }

    /// 程序化装配（测试 / TestApp / 内嵌场景）：跳过文件加载与 watcher。
    /// `settings.cache` 未显式选择后端时按默认 memory 装配。
    pub async fn from_settings(settings: Settings, environment: Environment) -> AppResult<Self> { // 按已就绪的设置装配
        // 日志最先初始化（后续步骤的日志才有输出；重复 init 安全忽略）
        let log_guard = crate::observability::logging::init(&settings); // 初始化日志并取得保活句柄

        // 安全规则启动期校验（fail-fast）：ip_filter 规则解析失败绝不允许静默降级
        if settings.server.ip_filter.enabled { // 若启用了 IP 过滤
            crate::middleware::ip_filter::IpFilter::from_rules( // 预解析规则以校验合法性
                &settings.server.ip_filter.allow, // 允许列表
                &settings.server.ip_filter.deny, // 拒绝列表
            )
            .map_err(|e| AppError::internal(format!("invalid [server.ip_filter] rule: {e}")))?; // 解析失败则报错终止
        }
        // CSRF 签名密钥：配置了就必须够长（<32 字节可被离线爆破，等于没签）
        if settings.server.csrf.enabled // 若启用 CSRF
            && !settings.server.csrf.secret.is_empty() // 且设置了密钥
            && settings.server.csrf.secret.len() < 32 // 且密钥长度不足 32 字节
        {
            return Err(AppError::internal( // 返回内部错误终止启动
                "[server.csrf].secret 过短：至少 32 字节（建议 openssl rand -base64 48，只走环境变量）", // 提示密钥过短
            ));
        }

        tracing::info!( // 打印启动日志
            environment = %environment, // 记录当前环境
            version = env!("CARGO_PKG_VERSION"), // 记录 crate 版本
            "core-rs bootstrapping" // 启动提示
        );

        // 数据库（fail-fast：连接失败直接退出）
        let db = if settings.database.enabled() { // 若配置了数据库
            Some(connect(&settings.database).await.map_err(|e| { // 建立连接池
                AppError::internal(format!("database connect failed: {e}")) // 连接失败转为内部错误
            })?)
        } else { // 未配置数据库
            None // 无连接池
        };

        // 缓存与锁（同一后端配对）
        let cache = build_cache(&settings.cache).map_err(cache_bootstrap_err)?; // 按配置构建缓存句柄
        let lock = build_lock(&settings.cache).map_err(cache_bootstrap_err)?; // 构建与缓存配对的锁句柄

        // 队列
        let queue = queue::build(&settings.queue).await?; // 按配置构建队列句柄

        // 认证链
        let auth = crate::auth::build(&settings.auth, &cache)?; // 按配置构建认证器组合

        #[cfg(feature = "session")] // 仅在开启 session 时装配会话管理器
        let sessions = if settings.auth.modes().iter().any(|m| m == "session") // 若认证方式含 session
            && cfg!(feature = "session") // 且编译期开启了 session
        {
            Some(Arc::new(crate::auth::session::SessionManager::new( // 创建会话管理器
                cache.clone(), // 复用缓存后端存会话
                &settings.auth.session, // 会话配置段
            )))
        } else { // 否则
            None // 无会话管理器
        };

        #[cfg(feature = "jwt")] // 仅在开启 jwt 时装配 JWT 签发器
        let jwt = if !settings.auth.jwt.secret.is_empty() { // 若配置了非空密钥
            Some(crate::auth::jwt::Jwt::new(&settings.auth.jwt)?) // 创建 JWT 签发器
        } else { // 否则
            None // 无 JWT 签发器
        };

        #[cfg(feature = "oauth2")] // 仅在开启 oauth2 时装配注册表
        let oauth2 = crate::auth::oauth2::OAuth2Registry::build(&settings.auth.oauth2)? // 按配置构建 OAuth2 注册表
            .map(Arc::new); // 用 Arc 包装（可能为 None）

        // Casbin（db 策略源需要连接池，先建连接再装配）
        #[cfg(feature = "casbin")] // 仅在开启 casbin 时装配强制器
        let authz = if settings.authz.enabled && settings.authz.auto_load { // 若启用授权且自动加载策略
            let mut adapter = // 构建策略适配器
                crate::authz::adapter::DbOrFileAdapter::from_settings(&settings.authz).await?; // 按配置选择 db 或文件源
            if settings.authz.source == "db" { // 若策略源为数据库
                if let Some(db) = &db { // 且已建连接
                    adapter.set_db(db.clone()); // 把连接池注入适配器
                }
            }
            // 把已注入连接的 adapter 传进去：Enforcer::new 会立即 load_policy，
            // 若在此处重新 from_settings，db 源会拿到 db=None 的空 adapter 而启动失败
            Some(Arc::new( // 构建强制器并包装为 Arc
                crate::authz::Enforcer::build_with(&settings.authz, adapter).await?, // 用已注入连接的适配器构建
            ))
        } else { // 否则
            None // 无授权强制器
        };

        #[cfg(any(feature = "ws", feature = "sse"))] // 开启 ws 或 sse 时装配实时通信 Hub
        let hub = { // 构建 Hub 并（可选）挂接跨实例转发
            let hub = Arc::new(crate::realtime::hub::Hub::new(&settings.realtime)); // 创建实时通信 Hub
            // 跨实例转发（v1 经 Redis Pub/Sub）：要求 cache-redis + cache.backend = redis
            #[cfg(feature = "cache-redis")] // 仅在开启 cache-redis 时启用转发
            if settings.realtime.enabled && settings.realtime.forward == "queue" { // 若启用实时且转发方式为 queue
                if settings.cache.backend == "redis" && settings.cache.redis.enabled() { // 且缓存后端为 redis
                    match crate::realtime::forward::Forwarder::start( // 启动 Redis Pub/Sub 转发器
                        hub.clone(), // 绑定 Hub
                        settings.realtime.forward_topic.clone(), // 转发 topic
                        &settings.cache.redis.url, // Redis 连接地址
                    )
                    .await // 等待启动结果
                    {
                        Ok(forwarder) => { // 启动成功
                            hub.set_forwarder(forwarder); // 把转发器挂到 Hub
                            tracing::info!("realtime cross-instance forwarding enabled (Redis Pub/Sub)"); // 记录已启用转发
                        }
                        Err(e) => { // 启动失败
                            tracing::warn!(error = %e, "realtime cross-instance forwarding disabled") // 告警并降级为不转发
                        }
                    }
                } else { // 缓存后端非 redis
                    tracing::warn!( // 告警配置不满足
                        "realtime.forward = \"queue\" requires cache.backend = \"redis\" (Redis Pub/Sub), forwarding disabled" // 提示需 redis 后端
                    );
                }
            }
            hub // 返回 Hub
        };

        tracing::info!( // 打印装配完成日志
            db = db.is_some(), // 是否已配置数据库
            cache_backend = settings.cache.backend.as_str(), // 缓存后端名
            queue_backend = queue.name(), // 队列后端名
            "core-rs bootstrap complete" // 完成提示
        );

        Ok(Self { // 组装核心状态
            environment, // 运行环境
            config: ConfigHandle::new(settings), // 用设置创建配置句柄
            db, // 数据库连接池
            cache, // 缓存句柄
            lock, // 锁句柄
            queue, // 队列句柄
            auth, // 认证器
            #[cfg(feature = "session")] // 仅在开启 session 时填字段
            sessions, // 会话管理器
            #[cfg(feature = "jwt")] // 仅在开启 jwt 时填字段
            jwt, // JWT 签发器
            #[cfg(feature = "oauth2")] // 仅在开启 oauth2 时填字段
            oauth2, // OAuth2 注册表
            #[cfg(feature = "casbin")] // 仅在开启 casbin 时填字段
            authz, // 授权强制器
            #[cfg(any(feature = "ws", feature = "sse"))] // 开启 ws 或 sse 时填字段
            hub, // 实时通信 Hub
            #[cfg(feature = "tls")] // 仅在开启 tls 时填字段
            tls: None, // TLS 状态由 App::serve 装配时注入
            health_checks: std::sync::RwLock::new(Vec::new()), // 探针列表初始为空
            _log_guard: Some(log_guard), // 持有日志保活句柄
        })
    }

    /// 注册应用自定义健康探针（/ready 聚合）
    pub fn register_health_check(&self, check: Arc<dyn crate::observability::health::HealthCheck>) { // 注册一个健康探针
        self.health_checks // 访问探针列表
            .write() // 获取写锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值
            .push(check); // 追加探针
    }
}

fn cache_bootstrap_err(e: CacheError) -> AppError { // 把缓存错误转为框架内部错误
    AppError::internal(format!("cache bootstrap failed: {e}")) // 包装错误信息
}
