//! 访问日志中间件（feature `access-log`）。
//!
//! 记录 `method` / `path` / `status` / `latency_ms` / `trace_id`；
//! 由 `[http].access_log.enabled` 控制是否启用。

use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::telemetry::current_trace_id;

/// 记录访问日志（`enabled = false` 时直接透传）。
pub(crate) async fn handle(enabled: bool, req: Request, next: Next) -> Response {
    if !enabled {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();

    let response = next.run(req).await;

    tracing::info!(
        target: "aurora::access",
        %method,
        %path,
        status = response.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        trace_id = %current_trace_id(),
        "access"
    );

    response
}
