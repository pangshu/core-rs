//! 访问日志：method / path / status / 耗时（在 trace span 内输出，自动携带
//! request_id / trace_id）。4xx 记 warn、5xx 记 error、其余 info。

use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

pub(crate) async fn handle(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // MatchedPath 在路由匹配后才存在（路由树内层），外层中间件取不到时退化为原始 path
    let matched = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string());

    let res = next.run(req).await;

    let status = res.status();
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let template = matched.as_deref().unwrap_or(&path);
    if status.is_server_error() {
        tracing::error!(method = %method, path = %template, status = status.as_u16(), elapsed_ms, "access");
    } else if status.is_client_error() {
        tracing::warn!(method = %method, path = %template, status = status.as_u16(), elapsed_ms, "access");
    } else {
        tracing::info!(method = %method, path = %template, status = status.as_u16(), elapsed_ms, "access");
    }
    res
}
