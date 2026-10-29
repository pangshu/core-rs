//! IP 黑白名单中间件（feature `ip-filter`）。
//!
//! 按 `[http].ip_filter` 的 `mode` / `cidrs` 放行或拒绝请求；受限时返回
//! `40002`（`IP_BLOCKED`，HTTP 403，见 `docs/09` §5）。
//!
//! 用 403 而不是 429：本层是「封禁」而非「超配额」。二者若共用 429，
//! WAF 规则与告警就只能靠响应体里的业务码区分，运维侧极易误判。
//!
//! **IP 来源**：`ClientIp::from_parts`（对端可信时才解析代理头，回退 TCP 对端），
//! 与限流 / geoip 保持同一套判定。白名单模式下**取不到 IP 时按拒绝处理**
//! （fail-closed），避免「拿不到 IP 就绕过白名单」。

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::IpFilterSection;
use crate::errors::{AppError, ErrorCode};
use crate::utils::ProxyTrust;
use crate::utils::client_ip::ClientIp;

/// 执行 IP 过滤（`mode = off` 时直接透传）。
pub(crate) async fn handle(
    rules: IpFilterSection,
    trust: ProxyTrust<'_>,
    req: Request,
    next: Next,
) -> Response {
    let ip = ClientIp::from_parts(req.headers(), req.extensions(), trust).0;
    if !rules.blocks(ip) {
        return next.run(req).await;
    }

    let shown = ip.map_or_else(|| "unknown".to_string(), |ip| ip.to_string());
    tracing::warn!(ip = %shown, mode = rules.mode.as_str(), "IP 被过滤规则拒绝");
    AppError::biz(ErrorCode::IP_BLOCKED, format!("IP 受限：{shown}")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IpFilterMode;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use ipnet::IpNet;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    fn rules(mode: IpFilterMode, cidrs: &[&str]) -> IpFilterSection {
        IpFilterSection {
            mode,
            cidrs: cidrs.iter().map(|cidr| cidr.parse().unwrap()).collect(),
            ..Default::default()
        }
    }

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|c| c.parse().unwrap()).collect()
    }

    fn router(rules: IpFilterSection, enabled: bool, trusted: Vec<IpNet>) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let rules = rules.clone();
                    let trusted = trusted.clone();
                    async move {
                        handle(rules, ProxyTrust::new(enabled, &trusted), req, next).await
                    }
                },
            ))
    }

    async fn call(router: Router, forwarded_for: Option<&str>) -> StatusCode {
        let mut builder = HttpRequest::builder().uri("/");
        if let Some(ip) = forwarded_for {
            builder = builder.header("x-forwarded-for", ip);
        }
        router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// 带 TCP 对端（`ConnectInfo`）的请求，用于验证「对端不可信时忽略 XFF」。
    async fn call_with_peer(
        router: Router,
        peer: [u8; 4],
        forwarded_for: Option<&str>,
    ) -> StatusCode {
        let mut builder = HttpRequest::builder()
            .uri("/")
            .extension(ConnectInfo(SocketAddr::from((peer, 4242))));
        if let Some(ip) = forwarded_for {
            builder = builder.header("x-forwarded-for", ip);
        }
        router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn should_pass_through_when_mode_off() {
        let app = router(rules(IpFilterMode::Off, &[]), true, Vec::new());
        assert_eq!(call(app, Some("203.0.113.9")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn should_deny_listed_ip_in_deny_mode() {
        let app = router(
            rules(IpFilterMode::Deny, &["203.0.113.0/24"]),
            true,
            nets(&[]),
        );
        assert_eq!(
            call_with_peer(app, [203, 0, 113, 9], None).await,
            StatusCode::FORBIDDEN,
            "黑名单命中 → 40002（403）"
        );
    }

    #[tokio::test]
    async fn should_allow_unlisted_ip_in_deny_mode() {
        let app = router(
            rules(IpFilterMode::Deny, &["203.0.113.0/24"]),
            true,
            nets(&[]),
        );
        assert_eq!(
            call_with_peer(app, [198, 51, 100, 1], None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn should_only_allow_listed_ip_in_allow_mode() {
        let app = router(
            rules(IpFilterMode::Allow, &["203.0.113.0/24"]),
            true,
            nets(&[]),
        );
        assert_eq!(
            call_with_peer(app, [203, 0, 113, 9], None).await,
            StatusCode::OK
        );

        let app = router(
            rules(IpFilterMode::Allow, &["203.0.113.0/24"]),
            true,
            nets(&[]),
        );
        assert_eq!(
            call_with_peer(app, [198, 51, 100, 1], None).await,
            StatusCode::FORBIDDEN
        );
    }

    /// 回归：链路里没有 `ConnectInfo` 时**无法校验对端**，转发头一律不采信。
    ///
    /// 修复前这里会按配置解析 XFF，于是「自建 server / 嵌入别的服务」这类部署
    /// 里，客户端自带一个 `X-Forwarded-For` 就能穿透白名单。
    #[tokio::test]
    async fn should_not_trust_forwarded_for_without_connect_info() {
        let app = router(
            rules(IpFilterMode::Allow, &["203.0.113.0/24"]),
            true,
            nets(&["10.0.0.0/8"]),
        );
        assert_eq!(
            call(app, Some("203.0.113.9")).await,
            StatusCode::FORBIDDEN,
            "无对端信息时伪造的 XFF 不得进白名单"
        );
    }

    #[tokio::test]
    async fn should_fail_closed_in_allow_mode_when_ip_unknown() {
        // 不信任代理头且无 ConnectInfo → IP 未知
        let app = router(
            rules(IpFilterMode::Allow, &["0.0.0.0/0"]),
            false,
            Vec::new(),
        );
        assert_eq!(call(app, Some("203.0.113.9")).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn should_ignore_forwarded_for_from_untrusted_peer() {
        // 直连对端 198.51.100.1 不在可信网段：伪造的 XFF 不能进白名单
        let app = router(
            rules(IpFilterMode::Allow, &["203.0.113.0/24"]),
            true,
            nets(&["10.0.0.0/8"]),
        );
        assert_eq!(
            call_with_peer(app, [198, 51, 100, 1], Some("203.0.113.9")).await,
            StatusCode::FORBIDDEN,
            "XFF 伪造必须被忽略"
        );
    }

    #[tokio::test]
    async fn should_trust_forwarded_for_from_trusted_peer() {
        // 对端 10.0.0.1 是可信代理：XFF 里的客户端 IP 生效
        let app = router(
            rules(IpFilterMode::Allow, &["203.0.113.0/24"]),
            true,
            nets(&["10.0.0.0/8"]),
        );
        assert_eq!(
            call_with_peer(app, [10, 0, 0, 1], Some("203.0.113.9")).await,
            StatusCode::OK
        );
    }
}
