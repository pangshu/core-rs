//! 内置中间件栈（按外到内）：
//! panic 捕获（统一 500）→ 压缩（可选）→ RequestId / TraceId 注入/回传 →
//! 请求 span（request_id / trace_id / service 字段进每条日志）→
//! 请求超时（408）→ 请求体大小限制 → CORS（可选）。

use std::time::Duration;

use axum::extract::Request;
use axum::http::{header::HeaderName, Method, StatusCode};
#[cfg(feature = "otel")]
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowMethods, AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::config::{CorsConfig, ServerConfig};
use crate::web::response::ApiResponse;

const REQUEST_ID_HEADER: &str = "x-request-id";
/// 日志追踪 id：缺失时每请求自动生成，上游传入则沿用（跨服务串联），响应头回传。
/// 与 [`REQUEST_ID_HEADER`] 的区别：request_id 面向"这次请求"（幂等/报障），
/// trace_id 面向"这条调用链的日志检索"（多实例/多服务聚合）
const TRACE_ID_HEADER: &str = "x-trace-id";
#[cfg(feature = "otel")]
const TRACEPARENT_HEADER: &str = "traceparent";

/// handler panic 的统一响应体（与内部错误同款，细节不外泄）
fn panic_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(ApiResponse::error(500, "internal server error")),
    )
        .into_response()
}

pub(crate) fn apply(app: Router, cfg: &ServerConfig, service_name: &str) -> Router {
    let request_id: HeaderName = REQUEST_ID_HEADER.parse().expect("static header name");
    let trace_id: HeaderName = TRACE_ID_HEADER.parse().expect("static header name");
    let service_name = service_name.to_string();

    let app = app.layer(
        ServiceBuilder::new()
            // panic 捕获放最外：handler panic 时客户端拿到统一 500 而不是连接重置
            .layer(CatchPanicLayer::custom(|_panic: Box<dyn std::any::Any + Send>| {
                tracing::error!("handler panicked");
                panic_response()
            }))
            // 两个 id 各自独立注入（缺失才生成，上游传入则保留）并回传响应头
            .layer(SetRequestIdLayer::new(request_id.clone(), MakeRequestUuid))
            .layer(PropagateRequestIdLayer::new(request_id))
            .layer(SetRequestIdLayer::new(trace_id.clone(), MakeRequestUuid))
            .layer(PropagateRequestIdLayer::new(trace_id))
            // 请求 span：字段进每条请求内日志（json 输出为 span 上下文字段）
            .layer(
                TraceLayer::new_for_http().make_span_with(move |req: &Request| {
                    tracing::info_span!(
                        "http_request",
                        service = %service_name,
                        method = %req.method(),
                        path = %req.uri().path(),
                        request_id = %header_value(req, REQUEST_ID_HEADER),
                        trace_id = %header_value(req, TRACE_ID_HEADER),
                    )
                }),
            )
            .layer(RequestBodyLimitLayer::new(cfg.body_limit.max(1)))
            // Timeout 必须最贴近路由：它要求内层响应体实现 Default，
            // 而 RequestBodyLimit 的响应体类型没有 Default 实现
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(cfg.request_timeout_secs.max(1)),
            )),
    );

    // otel 部署：上游按 W3C 规范传 traceparent 时，取其 trace-id 段作为本请求的
    // trace_id，使本地日志与导出的 OTel trace 直接互查
    #[cfg(feature = "otel")]
    let mut app = app.layer(axum::middleware::from_fn(adopt_traceparent));
    #[cfg(not(feature = "otel"))]
    let mut app = app;

    if cfg.compression.enabled {
        app = app.layer(CompressionLayer::new());
    }

    if cfg.cors.enabled {
        app = app.layer(cors_layer(&cfg.cors));
    }

    // 限流放最外层（CORS 之后），被限流的请求直接 429，不再进入后续链路
    #[cfg(feature = "rate-limit")]
    {
        app = crate::extra::rate_limit::apply(app, &cfg.rate_limit);
    }

    app
}

fn header_value(req: &Request, name: &str) -> String {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// 从 W3C traceparent（`<version>-<32位traceid>-<16位spanid>-<flags>`）提取 trace-id 段，
/// 全零 trace-id 按规范视为无效
#[cfg(feature = "otel")]
fn parse_traceparent_trace_id(tp: &str) -> Option<String> {
    let seg: Vec<&str> = tp.split('-').collect();
    match seg.as_slice() {
        [_version, trace_id, _span_id, _flags]
            if trace_id.len() == 32
                && trace_id.chars().all(|c| c.is_ascii_hexdigit())
                && trace_id.chars().any(|c| c != '0') =>
        {
            Some(trace_id.to_string())
        }
        _ => None,
    }
}

/// otel 部署：把合法 traceparent 的 trace-id 段写入 x-trace-id，
/// 使 span 字段、响应回传与 OTel 导出的 trace 一致
#[cfg(feature = "otel")]
async fn adopt_traceparent(
    mut req: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(trace_id) = req
        .headers()
        .get(TRACEPARENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_traceparent_trace_id)
    {
        if let Ok(value) = HeaderValue::from_str(&trace_id) {
            req.headers_mut().insert(TRACE_ID_HEADER, value);
        }
    }
    next.run(req).await
}

fn cors_layer(cfg: &CorsConfig) -> CorsLayer {
    let permissive =
        cfg.allow_origins.is_empty() || cfg.allow_origins.iter().any(|o| o == "*");

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
        AllowOrigin::list(
            cfg.allow_origins
                .iter()
                .filter_map(|o| o.parse().ok())
        )
    };

    let methods = if cfg.allow_methods.iter().any(|m| m == "*") {
        AllowMethods::any()
    } else {
        AllowMethods::list(
            cfg.allow_methods
                .iter()
                .filter_map(|m| m.parse::<Method>().ok())
        )
    };

    let headers = if cfg.allow_headers.iter().any(|h| h == "*") {
        tower_http::cors::AllowHeaders::any()
    } else {
        tower_http::cors::AllowHeaders::list(
            cfg.allow_headers
                .iter()
                .filter_map(|h| h.parse::<HeaderName>().ok())
        )
    };

    let expose = tower_http::cors::ExposeHeaders::list(
        cfg.expose_headers
            .iter()
            .filter_map(|h| h.parse::<HeaderName>().ok()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tower::ServiceExt;

    fn test_app() -> Router<()> {
        apply(
            axum::Router::new()
                .route("/ping", get(|| async { "ok" }))
                .with_state(crate::state::AppState::without_db(std::sync::Arc::new(
                    crate::config::AppConfig::default(),
                ))),
            &crate::config::ServerConfig::default(),
            "test-svc",
        )
    }

    fn get_req(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn header<'r>(res: &'r Response, name: &str) -> &'r str {
        res.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    #[tokio::test]
    async fn panic_maps_to_unified_500() {
        // 显式返回类型，避免 handler 输出类型落到 never fallback
        async fn boom() -> &'static str {
            panic!("boom")
        }
        let app = axum::Router::new()
            .route("/boom", get(boom))
            .with_state(crate::state::AppState::without_db(std::sync::Arc::new(
                crate::config::AppConfig::default(),
            )));
        let app = apply(app, &crate::config::ServerConfig::default(), "test-svc");

        let res = app.oneshot(get_req("/boom")).await.unwrap();

        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(r#""code":500"#), "response: {text}");
        assert!(text.contains("internal server error"), "response: {text}");
    }

    #[tokio::test]
    async fn trace_id_generated_and_distinct_from_request_id() {
        let res = test_app().oneshot(get_req("/ping")).await.unwrap();

        let request_id = header(&res, REQUEST_ID_HEADER);
        let trace_id = header(&res, TRACE_ID_HEADER);
        assert!(!request_id.is_empty(), "request_id 应自动生成");
        assert!(!trace_id.is_empty(), "trace_id 应自动生成");
        assert_ne!(trace_id, request_id, "两个 id 应相互独立");
    }

    #[tokio::test]
    async fn upstream_ids_are_preserved() {
        let req = axum::http::Request::builder()
            .uri("/ping")
            .header(REQUEST_ID_HEADER, "req-from-gateway")
            .header(TRACE_ID_HEADER, "trace-from-gateway")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = test_app().oneshot(req).await.unwrap();

        assert_eq!(header(&res, REQUEST_ID_HEADER), "req-from-gateway");
        assert_eq!(header(&res, TRACE_ID_HEADER), "trace-from-gateway");
    }

    #[tokio::test]
    async fn request_span_carries_ids_and_service() {
        use tracing_subscriber::{prelude::*, registry};

        type FieldMap = std::collections::HashMap<String, String>;

        struct FieldRecorder(FieldMap);
        impl tracing::field::Visit for FieldRecorder {
            fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                self.0.insert(f.name().to_string(), v.to_string());
            }
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.insert(f.name().to_string(), format!("{v:?}"));
            }
        }

        struct GlobalCapture;
        impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
            tracing_subscriber::Layer<S> for GlobalCapture
        {
            fn on_new_span(
                &self,
                attrs: &tracing::span::Attributes<'_>,
                _id: &tracing::span::Id,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut rec = FieldRecorder(Default::default());
                attrs.record(&mut rec);
                span_store().lock().unwrap().push(rec.0);
            }
        }

        // 必须用 set_global_default（触发 callsite interest 重建）；set_default 是
        // 线程级的，与全局 interest 缓存不兼容，并行测试下会静默丢失 span
        fn span_store() -> &'static std::sync::Mutex<Vec<FieldMap>> {
            static SPANS: std::sync::OnceLock<std::sync::Mutex<Vec<FieldMap>>> =
                std::sync::OnceLock::new();
            static INSTALL: std::sync::Once = std::sync::Once::new();
            let store = SPANS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
            INSTALL.call_once(|| {
                let _ = tracing::subscriber::set_global_default(registry().with(GlobalCapture));
            });
            store
        }

        let store = span_store();
        store.lock().unwrap().clear();

        let res = test_app().oneshot(get_req("/ping")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // 用响应头里的 trace_id 定位本次请求的 span，避免并行测试串扰
        let res_trace_id = header(&res, TRACE_ID_HEADER).to_string();
        let spans = store.lock().unwrap();
        let span = spans
            .iter()
            .find(|m| m.get("trace_id").map(|v| v.as_str()) == Some(res_trace_id.as_str()))
            .expect("http_request span 应携带与响应一致的 trace_id 字段");
        assert_eq!(span["service"], "test-svc");
        assert_eq!(span["path"], "/ping");
        assert!(!span["request_id"].is_empty());
        assert_ne!(span["trace_id"], span["request_id"], "双 id 应相互独立");
    }

    #[cfg(feature = "otel")]
    #[test]
    fn traceparent_parsing() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            parse_traceparent_trace_id(valid).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        // 全零 trace-id 按规范无效
        assert_eq!(parse_traceparent_trace_id("00-00000000000000000000000000000000-00f067aa0ba902b7-01"), None);
        assert_eq!(parse_traceparent_trace_id("00-short-00f067aa0ba902b7-01"), None);
        assert_eq!(parse_traceparent_trace_id("garbage"), None);
    }

    #[cfg(feature = "otel")]
    #[tokio::test]
    async fn traceparent_adopted_as_trace_id() {
        let req = axum::http::Request::builder()
            .uri("/ping")
            .header(
                TRACEPARENT_HEADER,
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .body(axum::body::Body::empty())
            .unwrap();
        let res = test_app().oneshot(req).await.unwrap();
        assert_eq!(
            header(&res, TRACE_ID_HEADER),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }
}
