//! 限流中间件实现。
//!
//! 超限返回统一响应体 `40001`（429），并带 `Retry-After`（秒）。
//!
//! ## 维度可信性
//!
//! 本层在 `auth` **外侧**（匿名流量也必须计入配额，见 `http/middleware/stack.rs`），
//! 因此 `req.extensions()` 里没有 `Identity`。`per_key` 的键只能来自
//! `verified_claims`：**只有通过 JWT 验签的令牌才算数**，验签失败 / 缺头一律
//! 回退到 IP 桶。否则客户端每请求换一个随机 `Authorization` 值就能拿到无限配额。
//!
//! ## 三条短路
//!
//! 1. `[ratelimit].enabled = false` → 直通（**且不做验签**：关掉限流就该零成本）；
//! 2. 路径在豁免清单里（健康检查 / 指标端点）→ 直通；
//! 3. 来源不可知（没有 `ConnectInfo`）→ 不建桶，走独立兜底配额（默认只告警）。

use axum::extract::Request;
#[cfg(feature = "auth")]
use axum::http::header::AUTHORIZATION;
use axum::http::header::{HeaderValue, RETRY_AFTER};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::errors::{AppError, ErrorCode};
use crate::ratelimit::RateLimitService;
use crate::utils::ProxyTrust;

/// 限流处理。
///
/// `auth` 只在 `per_key` 桶上用到（需要验签才能拿到可信主体）。
pub(crate) async fn handle(
    service: RateLimitService,
    #[cfg(feature = "auth")] auth: std::sync::Arc<crate::auth::AuthService>,
    trust: ProxyTrust<'_>,
    req: Request,
    next: Next,
) -> Response {
    if !service.enabled() {
        return next.run(req).await;
    }

    // 健康检查 / 指标端点必须豁免：它们与业务共用配额时，攻击者只要把配额打满，
    // 负载均衡的探活就会拿到 429 → 实例被摘除 → 全站雪崩。
    if service.is_excluded(req.uri().path()) {
        return next.run(req).await;
    }

    let bucket = service.bucket_for(req.uri().path(), req.method());

    // 只有需要主体的桶才验签：`per_ip` 不做多余的密码学运算
    #[cfg(feature = "auth")]
    let (req, subject) = {
        let mut req = req;
        let claims = verified_claims(&auth, req.headers(), bucket.strategy());
        if let Some(claims) = claims.as_ref() {
            req.extensions_mut()
                .insert(crate::auth::VerifiedClaims(claims.clone()));
        }
        (req, claims.map(|claims| claims.sub))
    };
    #[cfg(not(feature = "auth"))]
    let subject: Option<String> = None;

    let keys = service.keys_for(
        req.headers(),
        req.extensions(),
        trust,
        bucket.strategy(),
        subject.as_deref(),
    );

    match service.check_bucket(bucket, &keys).await {
        Ok(()) => next.run(req).await,
        Err(wait) => {
            let window_secs = bucket.window().as_secs().max(1);
            // 向上取整：`Retry-After: 59` 会让客户端早 1 秒重试又被拒，
            // 取整后落到窗口上界（同时夹在 `[1, window]` 内）。
            let seconds =
                (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).clamp(1, window_secs);
            tracing::debug!(
                key = keys.primary(),
                rule = bucket.name(),
                retry_after_secs = seconds,
                "触发限流"
            );

            let mut response =
                AppError::biz(ErrorCode::RATE_LIMITED, "请求过于频繁").into_response();
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
            response
        }
    }
}

/// 从 `Authorization: Bearer <jwt>` 中提取**已验签**的令牌载荷。
///
/// 仅在桶的 `strategy = per_key` 时才验签（`per_ip` 用不到主体，不做多余的密码学运算）。
///
/// 返回值同时用于两件事：
/// 1. 生成限流键（`claims.sub`）；
/// 2. 放进请求扩展 [`VerifiedClaims`](crate::auth::VerifiedClaims)，让内层的
///    `auth::authenticate` **复用**验签结果，避免同一请求验两次
///    （`RS256` 单次验签约 60µs，翻倍是实打实的开销）。
///
/// 验签失败 / 缺头 / 非 access 令牌 → `None`，调用方回退到 IP 桶。
#[cfg(feature = "auth")]
pub(crate) fn verified_claims(
    auth: &crate::auth::AuthService,
    headers: &axum::http::HeaderMap,
    strategy: crate::config::RateLimitStrategy,
) -> Option<crate::auth::jwt::Claims> {
    if strategy != crate::config::RateLimitStrategy::PerKey {
        return None;
    }
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = crate::auth::jwt::JwtCodec::bearer_token(value)?;
    auth.jwt().verify_access(token).ok()
}
