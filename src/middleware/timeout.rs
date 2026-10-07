//! 请求超时（tower-http TimeoutLayer 的配置包装）：超时返回 408。
//! 注意：该层要求响应体类型实现 Default，须挂在公共栈最内侧
//! （推荐顺序里紧贴 cors 之前，见 middleware/mod.rs）。

use std::time::Duration; // 引入时长类型

use axum::http::StatusCode; // 引入 HTTP 状态码类型
use tower_http::timeout::TimeoutLayer; // 引入 tower-http 超时层

pub fn layer(timeout_secs: u64) -> TimeoutLayer { // 按秒数构造超时层
    TimeoutLayer::with_status_code( // 指定超时返回的状态码
        StatusCode::REQUEST_TIMEOUT, // 超时返回 408
        Duration::from_secs(timeout_secs.max(1)), // 超时秒数（至少 1 秒）
    )
}
