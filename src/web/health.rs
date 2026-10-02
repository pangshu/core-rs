//! 内置健康检查端点：`/health` 存活探针、`/ready` 就绪探针（含 db / redis 组件状态）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;
use crate::web::response::{ApiResponse, ApiResult, CODE_OK};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
}

/// 存活探针：进程在即返回 ok
async fn health() -> ApiResult<&'static str> {
    Ok(ApiResponse::ok("ok"))
}

/// 就绪探针：逐一探测 db / redis 组件。任一**已配置**组件 down 时返回 503，
/// 便于 k8s / LB 就绪探针直接依据 HTTP 状态摘除实例；未配置的组件不参与判定。
async fn ready(State(state): State<AppState>) -> Response {
    let db = match &state.db {
        None => "not_configured".to_string(),
        Some(db) => match db.ping().await {
            Ok(()) => "up".to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "db ping failed");
                "down".to_string()
            }
        },
    };

    let cache = match &state.cache {
        None => "not_configured".to_string(),
        Some(cache) => match cache.ping().await {
            Ok(()) => "up".to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "redis ping failed");
                "down".to_string()
            }
        },
    };

    let degraded = db == "down" || cache == "down";
    let status = if degraded {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let body = ApiResponse {
        code: if degraded { 503 } else { CODE_OK },
        msg: if degraded {
            "component down".to_string()
        } else {
            "ok".to_string()
        },
        data: json!({ "db": db, "cache": cache }),
    };
    (status, Json(body)).into_response()
}
