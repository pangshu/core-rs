//! 把「层产出的裸状态码」归一成统一响应体。
//!
//! `tower_http` 的 `RequestBodyLimitLayer`（413）与 `TimeoutLayer`（408）只返回一个
//! 空 body 的状态码，违反 `docs/09` §1 的响应契约：客户端拿不到 `code` /
//! `trace_id`，也与提取器内部已映射好的 `10002` / `10003` 自相矛盾
//! （同一个「请求体过大」在两条路径上给出两种响应形状）。
//!
//! 这里在产生裸状态码的层**外侧**各加一层，把裸响应改写成统一错误体。
//! 已经是统一响应体（`application/json`）的响应原样放行——业务自己返回的
//! 413 / 408 不该被覆盖。

use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::errors::{AppError, ErrorCode};

/// 把 `RequestBodyLimitLayer` 产出的裸 413 改写成 `10002`。
pub(crate) async fn normalize_payload_too_large(req: Request, next: Next) -> Response {
    normalize(
        req,
        next,
        StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::PAYLOAD_TOO_LARGE,
    )
    .await
}

/// 把 `TimeoutLayer` 产出的裸 408 改写成 `10003`。
pub(crate) async fn normalize_request_timeout(req: Request, next: Next) -> Response {
    normalize(
        req,
        next,
        StatusCode::REQUEST_TIMEOUT,
        ErrorCode::REQUEST_TIMEOUT,
    )
    .await
}

async fn normalize(req: Request, next: Next, status: StatusCode, code: ErrorCode) -> Response {
    let response = next.run(req).await;
    if response.status() != status || is_unified(&response) {
        return response;
    }
    AppError::biz(code, code.default_message()).into_response()
}

/// 是否已经是统一响应体。
fn is_unified(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn call(app: Router) -> (StatusCode, serde_json::Value) {
        // 统一响应体的 trace_id 取自请求作用域（生产链路由 trace 中间件建立）
        crate::telemetry::scope(crate::telemetry::TraceId::new(), async move {
            let response = app
                .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        })
        .await
    }

    #[tokio::test]
    async fn should_rewrite_bare_413() {
        let app = Router::new()
            .route("/", get(|| async { StatusCode::PAYLOAD_TOO_LARGE }))
            .layer(axum::middleware::from_fn(normalize_payload_too_large));

        let (status, json) = call(app).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(json["code"], 10002);
        assert_eq!(json["trace_id"].as_str().map(str::len), Some(32));
    }

    #[tokio::test]
    async fn should_rewrite_bare_408() {
        let app = Router::new()
            .route("/", get(|| async { StatusCode::REQUEST_TIMEOUT }))
            .layer(axum::middleware::from_fn(normalize_request_timeout));

        let (status, json) = call(app).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(json["code"], 10003);
    }

    /// 已经是统一响应体的 413 不能被覆盖（否则会丢掉业务的 details）。
    #[tokio::test]
    async fn should_keep_existing_unified_response() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    AppError::biz_with_details(
                        ErrorCode::PAYLOAD_TOO_LARGE,
                        "自定义文案",
                        serde_json::json!({ "limit": 1024 }),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(normalize_payload_too_large));

        let (status, json) = call(app).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(json["message"], "自定义文案");
        assert_eq!(json["details"]["limit"], 1024);
    }

    #[tokio::test]
    async fn should_pass_through_other_statuses() {
        let app = Router::new()
            .route("/", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn(normalize_payload_too_large));

        let (status, _) = call(app).await;
        assert_eq!(status, StatusCode::OK);
    }
}
