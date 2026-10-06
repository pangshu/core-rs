//! 安全响应头：HSTS / X-Content-Type-Options / X-Frame-Options / CSP /
//! Referrer-Policy。值与开关由 `[server.security_headers]` 配置；
//! 值为空字符串的项不发送。

use std::sync::Arc;

use axum::Router;
use axum::http::header::{HeaderName, HeaderValue};

use crate::config::sections::SecurityHeadersSettings;

/// 按配置挂载安全响应头层
pub(crate) fn apply<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    cfg: &SecurityHeadersSettings,
) -> Router<S> {
    if !cfg.enabled {
        return router;
    }
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    let mut push = |name: &'static str, value: &str| {
        if !value.is_empty() {
            if let Ok(v) = HeaderValue::from_str(value) {
                headers.push((HeaderName::from_static(name), v));
            }
        }
    };
    push("x-content-type-options", "nosniff");
    push("x-frame-options", &cfg.frame_options);
    push("content-security-policy", &cfg.content_security_policy);
    push("referrer-policy", &cfg.referrer_policy);
    push("strict-transport-security", &cfg.hsts);
    let headers = Arc::new(headers);

    router.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
        let headers = headers.clone();
        async move {
            let mut res = next.run(req).await;
            for (name, value) in headers.iter() {
                res.headers_mut().insert(name, value.clone());
            }
            res
        }
        },
    ))
}
