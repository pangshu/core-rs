//! 安全响应头中间件（feature `security-headers`）。
//!
//! 注入基础安全响应头；由 `[http].security_headers` 控制。
//!
//! | 头 | 条件 |
//! |---|---|
//! | `x-content-type-options: nosniff` | 总开关开启 |
//! | `x-frame-options: DENY` | 总开关开启 |
//! | `referrer-policy: no-referrer` | 总开关开启 |
//! | `cache-control: no-store` | `no_store = true`（默认） |
//! | `strict-transport-security` | `hsts = true` **且**确认本次走的是 HTTPS |
//!
//! **HSTS 只在安全传输上输出**：按 RFC 6797 §8.1，浏览器会忽略明文 HTTP 上的
//! HSTS 头。应用内 TLS 终结时可直接判定为安全；前置网关终结 TLS 时只能靠
//! `X-Forwarded-Proto`，而该头**仅当直连对端是可信代理时**才采信（否则任何
//! 客户端都能伪造出「我是 HTTPS」）。
//!
//! CSP 默认策略尚未定案（见 `docs/06` 待决策项），暂不注入。

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, STRICT_TRANSPORT_SECURITY};
use axum::middleware::Next;
use axum::response::Response;

use crate::config::sections::SecurityHeadersSection;
use crate::utils::ProxyTrust;

/// 恒定注入的基础安全响应头（`name`, `value`）。
const HEADERS: [(&str, &str); 3] = [
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
];

/// 注入安全响应头。
///
/// `app_tls`：应用自身是否终结 TLS（`[tls].enabled`）。
pub(crate) async fn handle(
    cfg: SecurityHeadersSection,
    app_tls: bool,
    trust: ProxyTrust<'_>,
    req: Request,
    next: Next,
) -> Response {
    if !cfg.enabled {
        return next.run(req).await;
    }

    let secure = app_tls || is_secure_transport(trust, &req);
    let mut response = next.run(req).await;

    // 用 `entry().or_insert()`：业务在 handler 里显式设过的值优先
    let headers = response.headers_mut();
    for (name, value) in HEADERS {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    if cfg.no_store {
        headers
            .entry(CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("no-store"));
    }
    if cfg.hsts
        && secure
        && let Ok(value) = HeaderValue::from_str(&hsts_value(&cfg))
    {
        headers.entry(STRICT_TRANSPORT_SECURITY).or_insert(value);
    }
    response
}

/// 拼 `Strict-Transport-Security` 头值。
fn hsts_value(cfg: &SecurityHeadersSection) -> String {
    let mut value = format!("max-age={}", cfg.hsts_max_age.as_secs());
    if cfg.hsts_include_subdomains {
        value.push_str("; includeSubDomains");
    }
    if cfg.hsts_preload {
        value.push_str("; preload");
    }
    value
}

/// 本次请求是否走的是 HTTPS。
fn is_secure_transport(trust: ProxyTrust<'_>, req: &Request) -> bool {
    if req.uri().scheme_str() == Some("https") {
        return true;
    }
    // 前置网关终结 TLS：只有可信代理带来的 `X-Forwarded-Proto` 才算数
    if !trust.enabled() {
        return false;
    }
    let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
        return false;
    };
    if !trust.is_trusted(peer.ip()) {
        return false;
    }
    req.headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        // 多级代理会写成 `https, http`：最左侧才是客户端那一段
        .is_some_and(|value| {
            value
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("https")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use ipnet::IpNet;
    use tower::ServiceExt;

    fn config() -> SecurityHeadersSection {
        SecurityHeadersSection::default()
    }

    fn router(cfg: SecurityHeadersSection, app_tls: bool, trusted: &[&str]) -> Router {
        let nets: Vec<IpNet> = trusted.iter().map(|c| c.parse().unwrap()).collect();
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let cfg = cfg.clone();
                    let nets = nets.clone();
                    async move {
                        let trust = ProxyTrust::new(!nets.is_empty(), &nets);
                        handle(cfg, app_tls, trust, req, next).await
                    }
                },
            ))
    }

    async fn call(
        app: Router,
        peer: Option<[u8; 4]>,
        headers: &[(&str, &str)],
    ) -> axum::http::HeaderMap {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(peer) = peer {
            builder = builder.extension(ConnectInfo(SocketAddr::from((peer, 4242))));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.headers().clone()
    }

    #[tokio::test]
    async fn should_inject_base_headers_and_no_store() {
        let headers = call(router(config(), false, &[]), None, &[]).await;
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
        assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
        assert_eq!(headers.get(CACHE_CONTROL).unwrap(), "no-store");
    }

    #[tokio::test]
    async fn should_not_emit_hsts_over_plain_http() {
        let headers = call(router(config(), false, &[]), None, &[]).await;
        assert!(
            headers.get(STRICT_TRANSPORT_SECURITY).is_none(),
            "明文 HTTP 上的 HSTS 会被浏览器忽略，不应输出"
        );
    }

    #[tokio::test]
    async fn should_emit_hsts_when_app_terminates_tls() {
        let headers = call(router(config(), true, &[]), None, &[]).await;
        let value = headers
            .get(STRICT_TRANSPORT_SECURITY)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(value.contains("max-age=31536000"), "实际：{value}");
        assert!(value.contains("includeSubDomains"));
        assert!(!value.contains("preload"), "preload 默认关闭");
    }

    #[tokio::test]
    async fn should_trust_forwarded_proto_only_from_trusted_proxy() {
        // 直连客户端伪造 X-Forwarded-Proto：不得触发 HSTS
        let headers = call(
            router(config(), false, &["10.0.0.0/8"]),
            Some([203, 0, 113, 9]),
            &[("x-forwarded-proto", "https")],
        )
        .await;
        assert!(headers.get(STRICT_TRANSPORT_SECURITY).is_none());

        // 可信网关带 https：输出 HSTS
        let headers = call(
            router(config(), false, &["10.0.0.0/8"]),
            Some([10, 0, 0, 1]),
            &[("x-forwarded-proto", "https")],
        )
        .await;
        assert!(headers.get(STRICT_TRANSPORT_SECURITY).is_some());
    }

    #[tokio::test]
    async fn should_respect_switches() {
        let cfg = SecurityHeadersSection {
            hsts: false,
            no_store: false,
            ..config()
        };
        let headers = call(router(cfg, true, &[]), None, &[]).await;
        assert!(headers.get(STRICT_TRANSPORT_SECURITY).is_none());
        assert!(headers.get(CACHE_CONTROL).is_none());
        // 基础头仍然注入
        assert!(headers.get("x-frame-options").is_some());

        let disabled = SecurityHeadersSection {
            enabled: false,
            ..config()
        };
        let headers = call(router(disabled, true, &[]), None, &[]).await;
        assert!(headers.get("x-frame-options").is_none());
    }
}
