//! Prometheus 指标（feature = "metrics"）：
//! - `init()` 安装全局 recorder 并返回 `/metrics` 渲染句柄；
//! - `track()` 中间件统计请求数与耗时（按 method/path/status 维度）。

use std::time::Instant;

use axum::extract::Request;
use axum::response::Response;
use axum::routing::get;
use axum::{middleware::Next, Router};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::state::AppState;

/// 安装全局 Prometheus recorder（进程内只能安装一次）
pub fn init() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("prometheus recorder init failed (重复初始化？)")
}

/// /metrics 路由
pub fn routes(handle: PrometheusHandle) -> Router<AppState> {
    Router::new().route("/metrics", get(move || async move { handle.render() }))
}

/// 请求指标中间件：core_rs_http_requests_total / core_rs_http_request_duration_seconds
pub async fn track(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let resp = next.run(req).await;

    let status = resp.status().as_u16().to_string();
    metrics::counter!(
        "core_rs_http_requests_total",
        "method" => method.to_string(),
        "path" => path.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "core_rs_http_request_duration_seconds",
        "method" => method.to_string(),
        "path" => path,
    )
    .record(start.elapsed().as_secs_f64());

    resp
}
