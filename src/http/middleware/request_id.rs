//! 请求 ID 中间件。
//!
//! 生成 `x-request-id` 并回写到响应头，便于日志关联。
//!
//! **客户端不能指定请求 ID**。`tower_http` 的 `SetRequestIdLayer` 语义是
//! 「头已存在就不覆盖」，于是客户端自带一个 `X-Request-Id` 会被原样回显、
//! 并进入每一行日志：
//!
//! - 用固定值覆盖自己的调用链，把攻击流量混进他人链路（溯源失效）；
//! - 用含引号 / 空格的超长值破坏结构化日志解析、放大日志体积。
//!
//! 这里改为：**默认丢弃客户端值并重新生成**，只有直连对端属于
//! `[server].trusted_proxies` 时才继承上游值 —— 与 `trace` 的 trace-id 同口径
//! （见 `middleware/trace.rs`）。

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

use crate::utils::ProxyTrust;

/// 继承上游 ID 时的最大长度（它会被写进日志，越长放大越明显）。
const MAX_INHERITED_LEN: usize = 128;

/// 请求 ID 头名。
fn header_name() -> HeaderName {
    HeaderName::from_static("x-request-id")
}

/// 把请求头里的 ID 回写到响应头的 Layer。
///
/// 必须在 [`handle`] **内侧**：它读取的是已经规范化后的请求头。
pub(crate) fn propagate_layer() -> tower_http::request_id::PropagateRequestIdLayer {
    tower_http::request_id::PropagateRequestIdLayer::new(header_name())
}

/// 规范化 `x-request-id`：客户端值一律丢弃，除非对端是可信代理。
pub(crate) async fn handle(trust: ProxyTrust<'_>, mut req: Request, next: Next) -> Response {
    let id = resolve(trust, &req);
    if let Ok(value) = HeaderValue::from_str(&id) {
        req.headers_mut().insert(header_name(), value);
    }
    next.run(req).await
}

/// 决定本次请求的 request-id。
fn resolve(trust: ProxyTrust<'_>, req: &Request) -> String {
    if trust.enabled()
        && let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>()
        && trust.is_trusted(peer.ip())
        && let Some(inherited) = req
            .headers()
            .get(header_name())
            .and_then(|value| value.to_str().ok())
            .filter(|value| is_safe_id(value))
    {
        return inherited.to_string();
    }
    uuid::Uuid::new_v4().simple().to_string()
}

/// 上游 ID 只接受「可见 ASCII 且不过长」。
///
/// 控制字符能伪造日志行（日志注入），超长值能放大日志体积；两者都只可能来自
/// 客户端，即便对端可信也没必要照单全收。
fn is_safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_INHERITED_LEN
        && value.bytes().all(|byte| byte.is_ascii_graphic())
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

    fn router(trusted: &[&str], trust_proxy: bool) -> Router {
        let nets: Vec<IpNet> = trusted.iter().map(|c| c.parse().unwrap()).collect();
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(propagate_layer())
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let nets = nets.clone();
                    async move { handle(ProxyTrust::new(trust_proxy, &nets), req, next).await }
                },
            ))
    }

    async fn call(app: Router, peer: Option<[u8; 4]>, sent: Option<&str>) -> String {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(peer) = peer {
            builder = builder.extension(ConnectInfo(SocketAddr::from((peer, 4242))));
        }
        if let Some(value) = sent {
            builder = builder.header("x-request-id", value);
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// 回归：客户端自带的 `X-Request-Id` 必须被丢弃。
    #[tokio::test]
    async fn should_ignore_client_supplied_id() {
        let app = router(&["10.0.0.0/8"], true);
        let echoed = call(app, Some([203, 0, 113, 9]), Some("attacker-chosen")).await;

        assert_ne!(echoed, "attacker-chosen", "客户端值不得被回显");
        assert_eq!(echoed.len(), 32, "应回落到新生成的 uuid");
    }

    /// 对端不可信时，即便配了 `trusted_proxies` 也不继承。
    #[tokio::test]
    async fn should_ignore_id_from_untrusted_peer() {
        let app = router(&["10.0.0.0/8"], true);
        let echoed = call(app, Some([198, 51, 100, 1]), Some("upstream-id")).await;
        assert_ne!(echoed, "upstream-id");
    }

    #[tokio::test]
    async fn should_inherit_id_from_trusted_proxy() {
        let app = router(&["10.0.0.0/8"], true);
        let echoed = call(app, Some([10, 0, 0, 1]), Some("gw-123")).await;
        assert_eq!(echoed, "gw-123");
    }

    #[tokio::test]
    async fn should_reject_unsafe_id_even_from_trusted_proxy() {
        let long = "k".repeat(MAX_INHERITED_LEN + 1);
        for value in ["has space", long.as_str()] {
            let app = router(&["10.0.0.0/8"], true);
            let echoed = call(app, Some([10, 0, 0, 1]), Some(value)).await;
            assert_ne!(echoed, value, "非法上游值应被丢弃：{value}");
            assert_eq!(echoed.len(), 32);
        }
    }

    #[tokio::test]
    async fn should_generate_when_absent() {
        let app = router(&["10.0.0.0/8"], true);
        let echoed = call(app, Some([10, 0, 0, 1]), None).await;
        assert_eq!(echoed.len(), 32);
    }
}
