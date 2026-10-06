//! 装配帮手：路由树挂载、公共 layer 的推荐顺序（文档 三·6）。
//!
//! 推荐装配顺序（由外到内；`App::serve` 按此自动装配，应用自组装时保持一致）：
//!
//! ```text
//! panic → request_id → trace → locale → access_log → security_headers
//!       → body_limit → timeout → cors → ip_filter → rate_limit
//!       → idempotency → csrf → auth → authz
//! ```
//!
//! 请求体大小限制用 axum 内置的 `DefaultBodyLimit`，不单独成文件；
//! 超时用 `tower-http` 的 `TimeoutLayer`（要求最贴近路由，放在公共栈最内侧）。

use std::time::Duration;

use axum::Router;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowMethods, AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::config::sections::{CorsSettings, ServerSettings};

/// 把若干棵路由树合并为一棵（挂载顺序即合并顺序，路径冲突时启动期即报错）
pub fn mount<S: Clone + Send + Sync + 'static>(base: Router<S>, trees: Vec<Router<S>>) -> Router<S> {
    trees.into_iter().fold(base, |acc, tree| acc.merge(tree))
}

/// 公共层装配（无状态件）：panic → request_id → locale → trace → access_log
/// → security_headers → body_limit → timeout。压缩与 CORS 在外层单独挂
/// （见 [`compression_layer`] / [`cors_layer`]）。
///
/// locale 必须在 request_id **之后**协商（RequestContext 已插入），
/// 放在这里而不是 App 最外层——外层先执行，会读不到还没创建的 RequestContext。
pub(crate) fn base_layers<S: Clone + Send + Sync + 'static + crate::traits::HasConfig>(
    router: Router<S>,
    state: S,
    settings: &ServerSettings,
    service_name: &str,
) -> Router<S> {
    let service_name = service_name.to_string();
    let router = router.layer(
        ServiceBuilder::new()
            // panic 捕获放最外：handler panic 时客户端拿到统一 500 而不是连接重置
            .layer(CatchPanicLayer::custom(|_panic: Box<dyn std::any::Any + Send>| {
                tracing::error!("handler panicked");
                crate::middleware::panic::panic_response()
            }))
            // request_id / trace_id 双 id（见 web/context.rs 模块注释）
            .layer(axum::middleware::from_fn(crate::middleware::request_id::handle))
            // locale 协商在 request_id 之后：RequestContext 已由上一段插入
            //（i18n feature 关闭时为 no-op，见 middleware/locale.rs）
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::locale::handle::<S>,
            ))
            .layer(
                TraceLayer::new_for_http().make_span_with(move |req: &axum::extract::Request| {
                    tracing::info_span!(
                        "http_request",
                        service = %service_name,
                        method = %req.method(),
                        path = %req.uri().path(),
                        request_id = %crate::middleware::request_id::header_value(req),
                        trace_id = %crate::middleware::request_id::trace_header_value(req),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(crate::middleware::access_log::handle)),
    );

    let router = crate::middleware::security_headers::apply(router, &settings.security_headers);
    let router = router.layer(RequestBodyLimitLayer::new(settings.body_limit.max(1)));
    router.layer(TimeoutLayer::with_status_code(
        axum::http::StatusCode::REQUEST_TIMEOUT,
        Duration::from_secs(settings.request_timeout_secs.max(1)),
    ))
}

/// 压缩层（配置开关）
pub(crate) fn compression_layer<S: Clone + Send + Sync + 'static>(router: Router<S>, enabled: bool) -> Router<S> {
    if enabled {
        router.layer(CompressionLayer::new())
    } else {
        router
    }
}

/// CORS 层（配置开关）
pub(crate) fn cors_layer<S: Clone + Send + Sync + 'static>(router: Router<S>, cfg: &CorsSettings) -> Router<S> {
    if !cfg.enabled {
        return router;
    }
    router.layer(build_cors(cfg))
}

pub fn build_cors(cfg: &CorsSettings) -> CorsLayer {
    let permissive = cfg.allow_origins.is_empty() || cfg.allow_origins.iter().any(|o| o == "*");

    // credentials 与通配 origins 组合是无效 CORS（tower-http 会 panic），降级并提示
    let credentials = if cfg.allow_credentials && permissive {
        tracing::warn!("cors.allow_credentials=true requires explicit allow_origins (not \"*\"), ignoring credentials");
        false
    } else {
        cfg.allow_credentials
    };

    let origins = if permissive {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(cfg.allow_origins.iter().filter_map(|o| o.parse().ok()))
    };

    let methods = if cfg.allow_methods.iter().any(|m| m == "*") {
        AllowMethods::any()
    } else {
        AllowMethods::list(
            cfg.allow_methods
                .iter()
                .filter_map(|m| m.parse::<axum::http::Method>().ok()),
        )
    };

    let headers = if cfg.allow_headers.iter().any(|h| h == "*") {
        tower_http::cors::AllowHeaders::any()
    } else {
        tower_http::cors::AllowHeaders::list(
            cfg.allow_headers
                .iter()
                .filter_map(|h| h.parse::<axum::http::header::HeaderName>().ok()),
        )
    };

    let expose = tower_http::cors::ExposeHeaders::list(
        cfg.expose_headers
            .iter()
            .filter_map(|h| h.parse::<axum::http::header::HeaderName>().ok()),
    );

    let mut layer = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(methods)
        .allow_headers(headers)
        .expose_headers(expose)
        .allow_credentials(credentials);

    if cfg.max_age_secs > 0 {
        layer = layer.max_age(Duration::from_secs(cfg.max_age_secs));
    }
    layer
}

/// `ServerSettings` 的公共层全量装配（App::serve 使用；按推荐顺序自外向内）。
/// 状态相关件（ip_filter / rate_limit / idempotency / csrf / auth）由 app.rs
/// 在拿到状态后挂载；authz 经路由级 `required()` 声明即校验。
#[allow(unused_variables)]
pub(crate) fn assemble_base<S: Clone + Send + Sync + 'static + crate::traits::HasConfig>(
    router: Router<S>,
    state: S,
    settings: &ServerSettings,
    service_name: &str,
) -> Router<S> {
    let router = cors_layer(router, &settings.cors);
    let router = compression_layer(router, settings.compression.enabled);
    base_layers(router, state, settings, service_name)
}
