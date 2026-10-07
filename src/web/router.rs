//! 装配帮手：路由树挂载、公共 layer 的推荐顺序（文档 三·6）。
//!
//! 推荐装配顺序（由外到内；`App::serve` 默认模式按此自动装配）：
//!
//! ```text
//! Extension(CoreState) → timeout → body_limit → security_headers → panic
//!       → request_id → locale → trace → access_log → compression → cors
//!       → [metrics::track] → ip_filter → rate_limit → csrf → auth → idempotency
//! ```
//!
//! 裸骨架模式（`App::bare`）只保留三件框架必需件（Extension(CoreState) /
//! panic / request_id），其余由应用按树自选（见 middleware/stack.rs 与各
//! 中间件模块的 layer 构造器）。
//!
//! 请求体大小限制用 axum 内置的 `DefaultBodyLimit` 形态（RequestBodyLimitLayer），
//! 不单独成文件；超时用 `tower-http` 的 `TimeoutLayer`（要求最贴近路由，放在
//! 公共栈外侧）。

use std::time::Duration; // 引入时长类型，用于超时与 CORS max-age

use axum::Router; // 引入 axum 路由类型
use tower::ServiceBuilder; // 引入 tower 的服务层构建器，按序组合 layer
use tower_http::compression::CompressionLayer; // 引入响应压缩层
use tower_http::cors::{AllowMethods, AllowOrigin, CorsLayer}; // 引入 CORS 层与允许方法/来源构造器
use tower_http::limit::RequestBodyLimitLayer; // 引入请求体大小限制层
use tower_http::timeout::TimeoutLayer; // 引入请求超时层
use tower_http::trace::TraceLayer; // 引入追踪 span 层

use crate::config::sections::{CorsSettings, ServerSettings}; // 引入 CORS 与服务端配置节

/// 把若干棵路由树合并为一棵（挂载顺序即合并顺序，路径冲突时启动期即报错）
pub fn mount<S: Clone + Send + Sync + 'static>(base: Router<S>, trees: Vec<Router<S>>) -> Router<S> { // 定义路由树合并帮手
    trees.into_iter().fold(base, |acc, tree| acc.merge(tree)) // 依次把各路由树合并进基准路由
}

/// 公共层装配（无状态件）：panic → request_id → locale → trace → access_log
/// → security_headers → body_limit → timeout（请求流由外到内）。压缩与 CORS
/// 在内侧单独挂（见 [`compression_layer`] / [`cors_layer`]）。
///
/// locale 必须在 request_id **之后**协商（RequestContext 已插入），
/// 放在这里而不是 App 最外层——外层先执行，会读不到还没创建的 RequestContext。
pub fn base_layers<S: Clone + Send + Sync + 'static>( // 定义公共无状态层装配函数
    router: Router<S>, // 待装配的路由
    settings: &ServerSettings, // 服务端配置（安全头/体积/超时）
    service_name: &str, // 服务名，用于 trace span 标签
) -> Router<S> { // 返回装配后的路由
    let service_name = service_name.to_string(); // 复制服务名供闭包 move 捕获
    let router = router.layer( // 用 ServiceBuilder 组合若干层（先挂者为外）
        ServiceBuilder::new() // 创建层构建器
            // request_id / trace_id 双 id（见 web/context.rs 模块注释）
            .layer(axum::middleware::from_fn(crate::middleware::request_id::handle)) // 挂载双 id 中间件
            // locale 协商在 request_id 之后：RequestContext 已由上一段插入
            //（i18n feature 关闭时为 no-op，见 middleware/locale.rs；
            // ServiceBuilder 组合栈内不能放 BoxedLayer 构造器，直接引 handle）
            .layer(axum::middleware::from_fn(crate::middleware::locale::handle)) // 挂载 locale 协商层
            .layer( // 挂载 TraceLayer
                TraceLayer::new_for_http().make_span_with(move |req: &axum::extract::Request| { // 自定义每个请求的 span
                    tracing::info_span!( // 创建 info 级 span
                        "http_request", // span 名称
                        service = %service_name, // 记录服务名
                        method = %req.method(), // 记录请求方法
                        path = %req.uri().path(), // 记录请求路径
                        request_id = %crate::middleware::request_id::header_value(req), // 记录请求 id 头
                        trace_id = %crate::middleware::request_id::trace_header_value(req), // 记录追踪 id 头
                    )
                }),
            )
            .layer(axum::middleware::from_fn(crate::middleware::access_log::handle)), // 挂载访问日志中间件
    );
    // panic 捕获在 builder 之后挂：位于 request_id 外侧（与原推荐位次一致）、
    // security_headers 内侧——handler 与内层中间件 panic 时客户端拿到统一 500
    let router = crate::middleware::panic::layer(router); // 挂载 panic 捕获层

    let router = crate::middleware::security_headers::apply(router, &settings.security_headers); // 按配置挂载安全响应头层
    let router = router.layer(RequestBodyLimitLayer::new(settings.body_limit.max(1))); // 挂载请求体大小限制（至少 1 字节）
    router.layer(TimeoutLayer::with_status_code( // 挂载超时层并指定超时状态码
        axum::http::StatusCode::REQUEST_TIMEOUT, // 超时返回 408
        Duration::from_secs(settings.request_timeout_secs.max(1)), // 超时秒数（至少 1 秒）
    ))
}

/// 压缩层（配置开关）
pub fn compression_layer<S: Clone + Send + Sync + 'static>(router: Router<S>, enabled: bool) -> Router<S> { // 定义压缩层装配
    if enabled { // 配置开启压缩时
        router.layer(CompressionLayer::new()) // 挂载响应压缩层
    } else {
        router // 未开启则原样返回
    }
}

/// CORS 层（配置开关）
pub fn cors_layer<S: Clone + Send + Sync + 'static>(router: Router<S>, cfg: &CorsSettings) -> Router<S> { // 定义 CORS 层装配
    if !cfg.enabled { // CORS 未开启时
        return router; // 直接返回不挂载
    }
    router.layer(build_cors(cfg)) // 按配置构造并挂载 CORS 层
}

pub fn build_cors(cfg: &CorsSettings) -> CorsLayer { // 由配置构造 CorsLayer
    let permissive = cfg.allow_origins.is_empty() || cfg.allow_origins.iter().any(|o| o == "*"); // 判断是否通配放行来源

    // credentials 与通配 origins 组合是无效 CORS（tower-http 会 panic），降级并提示
    let credentials = if cfg.allow_credentials && permissive { // 若同时开启凭证与通配来源
        tracing::warn!("cors.allow_credentials=true requires explicit allow_origins (not \"*\"), ignoring credentials"); // 告警并降级
        false // 关闭凭证支持，避免 tower-http panic
    } else {
        cfg.allow_credentials // 否则按配置决定
    };

    let origins = if permissive { // 通配模式
        AllowOrigin::any() // 允许任意来源
    } else {
        AllowOrigin::list(cfg.allow_origins.iter().filter_map(|o| o.parse().ok())) // 仅允许配置中可解析的来源
    };

    let methods = if cfg.allow_methods.iter().any(|m| m == "*") { // 方法含通配
        AllowMethods::any() // 允许任意方法
    } else {
        AllowMethods::list( // 仅允许配置中可解析的方法
            cfg.allow_methods
                .iter()
                .filter_map(|m| m.parse::<axum::http::Method>().ok()),
        )
    };

    let headers = if cfg.allow_headers.iter().any(|h| h == "*") { // 请求头含通配
        tower_http::cors::AllowHeaders::any() // 允许任意请求头
    } else {
        tower_http::cors::AllowHeaders::list( // 仅允许配置中可解析的请求头
            cfg.allow_headers
                .iter()
                .filter_map(|h| h.parse::<axum::http::header::HeaderName>().ok()),
        )
    };

    let expose = tower_http::cors::ExposeHeaders::list( // 暴露给浏览器的响应头
        cfg.expose_headers
            .iter()
            .filter_map(|h| h.parse::<axum::http::header::HeaderName>().ok()),
    );

    let mut layer = CorsLayer::new() // 创建 CORS 层并链式配置
        .allow_origin(origins) // 设置允许来源
        .allow_methods(methods) // 设置允许方法
        .allow_headers(headers) // 设置允许请求头
        .expose_headers(expose) // 设置暴露响应头
        .allow_credentials(credentials); // 设置是否允许凭证

    if cfg.max_age_secs > 0 { // 若配置了预检缓存时长
        layer = layer.max_age(Duration::from_secs(cfg.max_age_secs)); // 设置预检结果缓存时长
    }
    layer // 返回构造好的 CORS 层
}

/// `ServerSettings` 的公共层全量装配（App::serve 默认模式使用；按推荐顺序
/// 自外向内）。状态相关件（ip_filter / rate_limit / csrf / auth / idempotency）
/// 由 app.rs 在此之前挂载（依赖最外层的 `Extension(CoreState)`）；
/// authz 经路由级 `required()` 声明即校验。
pub fn assemble_base<S: Clone + Send + Sync + 'static>( // 定义公共层全量装配入口
    router: Router<S>, // 待装配的路由
    settings: &ServerSettings, // 服务端配置
    service_name: &str, // 服务名（trace 标签）
) -> Router<S> { // 返回装配后的路由
    let router = cors_layer(router, &settings.cors); // 先挂 CORS 层（最内圈）
    let router = compression_layer(router, settings.compression.enabled); // 再挂压缩层
    base_layers(router, settings, service_name) // 最后挂公共无状态层并返回
}
