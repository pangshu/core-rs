//! 超时中间件。
//!
//! 仅作用于**普通请求**；SSE / WS 需豁免（见 `docs/02 §3`）。
//! 超时返回 `408`（对应错误码 `10003`）。

use std::time::Duration;

use axum::http::StatusCode;
use tower_http::timeout::TimeoutLayer;

/// 构造超时 Layer（超时返回 `408 Request Timeout`）。
pub(crate) fn layer(timeout: Duration) -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, timeout)
}
