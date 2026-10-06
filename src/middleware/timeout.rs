//! 请求超时（tower-http TimeoutLayer 的配置包装）：超时返回 408。
//! 注意：该层要求响应体类型实现 Default，须挂在公共栈最内侧
//! （推荐顺序里紧贴 cors 之前，见 middleware/mod.rs）。

use std::time::Duration;

use axum::http::StatusCode;
use tower_http::timeout::TimeoutLayer;

pub fn layer(timeout_secs: u64) -> TimeoutLayer {
    TimeoutLayer::with_status_code(
        StatusCode::REQUEST_TIMEOUT,
        Duration::from_secs(timeout_secs.max(1)),
    )
}
