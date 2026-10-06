//! Prometheus 指标（feature = "metrics"，文档 三·8）：
//! QPS / 延迟 / 错误率经请求计数与耗时直方图暴露 `/metrics`；
//! 关闭 feature 时零开销。

use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// 安装全局 Prometheus recorder（进程内只能安装一次；重复调用返回 None）
pub fn init() -> Option<PrometheusHandle> {
    PrometheusBuilder::new().install_recorder().ok()
}

/// 请求指标中间件：
/// `core_rs_http_requests_total{method,path,status}` /
/// `core_rs_http_request_duration_seconds{method,path}`
pub async fn track(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    // MatchedPath 在路由匹配后才存在（路由树内层）；未匹配（404）时**必须**归一化：
    // path 是 Prometheus label，按值去重存储，用户可控的原始路径会制造无上界时间序列
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "__unmatched__".to_string());

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

/// /metrics 响应体渲染
pub fn render(handle: &PrometheusHandle) -> String {
    handle.render()
}
