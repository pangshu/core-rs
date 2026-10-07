//! 预设栈帮手（裸模式自组装用）：把公共无状态层按推荐顺序打包，应用一棵
//! 路由树一次收尾，无需背整条推荐顺序；想微调可在打包后再叠层，或拆开用
//! 各模块的 layer 构造器（见 middleware/mod.rs 头部文档）。
//!
//! 框架必需件（panic 捕获 / request_id / `Extension(CoreState)`）不在其中——
//! `App::serve` 两种模式下都自动挂载，放进来反而会重复。
//!
//! tower 层不可内省，顺序错误无法启动期校验；本帮手的存在就是把"顺序
//! 正确"固化在框架侧，应用只负责决定"哪些树要哪些状态件"。

use axum::Router; // 引入 axum 路由类型
use tower_http::limit::RequestBodyLimitLayer; // 引入请求体大小限制层
use tower_http::trace::TraceLayer; // 引入追踪 span 层

use crate::config::sections::ServerSettings; // 引入服务端配置节（超时/体积/安全头/CORS/压缩）

/// 公共无状态层打包：timeout → body_limit → security_headers → cors →
/// compression → locale → trace → access_log（请求流由外到内）。
/// 各件是否生效由 `[server]` 对应配置节决定（关闭即 no-op，支持热更新）；
/// trace span 的 service 标签从请求 extension 里的 `CoreState` 读取（缺失时
/// 退化为 "core-rs"）。
///
/// 用法（状态件由内向外挂，本函数收尾兜住外圈）：
///
/// ```rust,ignore
/// let admin = stack::common(
///     admin::routes()
///         .layer(auth::require_identity_layer())
///         .layer(auth::layer()),
///     &s.server,
/// );
/// ```
pub fn common<S: Clone + Send + Sync + 'static>( // 定义公共无状态层打包函数
    router: Router<S>, // 待装配的路由（通常已挂好业务自选的状态件）
    server: &ServerSettings, // 服务端配置（取各件参数与开关）
) -> Router<S> { // 返回装配后的路由
    // 由内向外逐层叠加（axum `Router::layer` 后挂者为外层）：
    // access_log 最内（贴近路由，落在业务状态件外侧与默认模式一致）
    let router = router.layer(axum::middleware::from_fn(crate::middleware::access_log::handle)); // 挂载访问日志层
    // trace span：service 标签运行期经 extension 读配置（热更新可跟随）
    let router = router.layer(TraceLayer::new_for_http().make_span_with(|req: &axum::extract::Request| { // 自定义每个请求的 span
        let service = req // 从请求扩展取核心状态
            .extensions() // 访问扩展
            .get::<crate::state::CoreState>() // 取 CoreState
            .map(|core| core.config.load().service_name().to_string()) // 读服务名
            .unwrap_or_else(|| "core-rs".to_string()); // 缺扩展时退化
        tracing::info_span!( // 创建 info 级 span
            "http_request", // span 名称
            service = %service, // 记录服务名
            method = %req.method(), // 记录请求方法
            path = %req.uri().path(), // 记录请求路径
            request_id = %crate::middleware::request_id::header_value(req), // 记录请求 id 头
            trace_id = %crate::middleware::request_id::trace_header_value(req), // 记录追踪 id 头
        )
    }));
    // locale 协商须在 request_id 之后（框架必需件在外侧已插入 RequestContext）
    let router = router.layer(crate::middleware::locale::layer()); // 挂载 locale 协商层
    let router = crate::web::router::compression_layer(router, server.compression.enabled); // 按配置挂载压缩层
    let router = crate::web::router::cors_layer(router, &server.cors); // 按配置挂载 CORS 层
    let router = crate::middleware::security_headers::apply(router, &server.security_headers); // 按配置挂载安全响应头层
    let router = router.layer(RequestBodyLimitLayer::new(server.body_limit.max(1))); // 挂载请求体大小限制（至少 1 字节）
    // 超时层贴着公共栈外侧（与默认模式位次一致）
    router.layer(crate::middleware::timeout::layer(server.request_timeout_secs)) // 挂载超时层并返回
}
