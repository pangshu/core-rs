//! health（文档 三·8）：`/health`（liveness，进程在即 ok）与 `/ready`
//! （readiness，依次探 DB 与已启用的缓存/队列后端，任一失败 503）。
//! 探针项通过 [`HealthCheck`] trait 注册（`CoreState::register_health_check`），
//! 应用可追加自定义探针（如对象存储）。
//!
//! health/metrics 路由由框架在 `App::serve()` 时自动挂载，应用无需关心。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;

use crate::traits::{HasCache, HasDb, HasHealthChecks, HasQueue};

/// 自定义探针契约：name 用于 /ready 的 custom 字段，check 返回存活状态
#[async_trait::async_trait]
pub trait HealthCheck: Send + Sync {
    fn name(&self) -> &str;
    async fn check(&self) -> HealthStatus;
}

/// 探针结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Up,
    Down,
}

/// /health（liveness）：进程在即返回 ok
pub async fn liveness() -> Response {
    (
        StatusCode::OK,
        Json(crate::web::response::ApiResponse::ok("ok")),
    )
        .into_response()
}

/// /ready（readiness）：逐一探测 db / cache / queue 与注册的自定义探针。
/// 任一**已配置**组件 down 时返回 503，便于 k8s / LB 就绪探针直接摘除实例；
/// 未配置的组件不参与判定。
pub(crate) async fn readiness<S>(State(state): State<S>) -> Response
where
    S: HasDb + HasCache + HasQueue + HasHealthChecks + Clone + Send + Sync + 'static,
{
    let db = match state.db() {
        None => "not_configured".to_string(),
        Some(db) => match db.ping().await {
            Ok(()) => "up".to_string(),
            Err(e) => {
                tracing::warn!(error = %e, "db ping failed");
                "down".to_string()
            }
        },
    };

    let cache = match state.cache().ping().await {
        Ok(()) => "up".to_string(),
        Err(e) => {
            tracing::warn!(error = %e, "cache ping failed");
            "down".to_string()
        }
    };

    let queue = match state.queue().ping().await {
        Ok(()) => "up".to_string(),
        Err(e) => {
            tracing::warn!(error = %e, "queue ping failed");
            "down".to_string()
        }
    };

    let mut custom = Vec::new();
    for check in state.health_checks() {
        let status = match check.check().await {
            HealthStatus::Up => "up".to_string(),
            HealthStatus::Down => "down".to_string(),
        };
        custom.push((check.name().to_string(), status));
    }

    let degraded = db == "down"
        || cache == "down"
        || queue == "down"
        || custom.iter().any(|(_, s)| s == "down");
    let status = if degraded {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let body = crate::web::response::ApiResponse {
        code: if degraded { 503 } else { crate::web::response::CODE_OK },
        message: if degraded {
            "component down".to_string()
        } else {
            "ok".to_string()
        },
        data: json!({
            "db": db,
            "cache": cache,
            "queue": queue,
            "custom": custom,
        }),
    };
    (status, Json(body)).into_response()
}

/// 框架健康路由（泛型状态）：/health + /ready
pub fn routes<S>() -> Router<S>
where
    S: HasDb + HasCache + HasQueue + HasHealthChecks + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/health", axum::routing::get(liveness))
        .route("/ready", axum::routing::get(readiness::<S>))
}

// Arc 供 HealthCheck 注册与读取使用
#[allow(unused)]
type HealthCheckHandle = Arc<dyn HealthCheck>;
