//! 请求体大小限制中间件。
//!
//! 超限由 tower-http 直接拒绝（413）；统一错误体的收口在 M1 完成。

use tower_http::limit::RequestBodyLimitLayer;

/// 构造请求体大小限制 Layer。
pub(crate) fn layer(max_bytes: usize) -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(max_bytes)
}
