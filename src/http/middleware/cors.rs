//! CORS 中间件。
//!
//! 默认策略收紧：未配置 `allowed_origins` 时不放开任何跨域来源。

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{Any, CorsLayer};

use crate::config::sections::CorsSection;

/// 由配置构造 CORS Layer。
pub(crate) fn layer(cfg: &CorsSection) -> CorsLayer {
    let mut layer = CorsLayer::new();

    if cfg.allowed_origins.iter().any(|o| o == "*") {
        layer = layer.allow_origin(Any);
    } else if !cfg.allowed_origins.is_empty() {
        let origins: Vec<HeaderValue> = cfg
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse::<HeaderValue>().ok())
            .collect();
        if !origins.is_empty() {
            layer = layer.allow_origin(origins);
        }
    }

    if !cfg.allowed_methods.is_empty() {
        let methods: Vec<Method> = cfg
            .allowed_methods
            .iter()
            .filter_map(|m| m.parse::<Method>().ok())
            .collect();
        if !methods.is_empty() {
            layer = layer.allow_methods(methods);
        }
    }

    if cfg.allowed_headers.iter().any(|h| h == "*") {
        layer = layer.allow_headers(Any);
    } else if !cfg.allowed_headers.is_empty() {
        let headers: Vec<HeaderName> = cfg
            .allowed_headers
            .iter()
            .filter_map(|h| h.parse::<HeaderName>().ok())
            .collect();
        if !headers.is_empty() {
            layer = layer.allow_headers(headers);
        }
    }

    if cfg.max_age > Duration::ZERO {
        layer = layer.max_age(cfg.max_age);
    }

    layer
}
