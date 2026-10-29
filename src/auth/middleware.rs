//! 认证中间件。
//!
//! [`authenticate`] 从 `Authorization: Bearer <token>` 解析身份并写入请求扩展，
//! 供 [`Principal`](crate::auth::Principal) 等提取器读取。
//!
//! 行为：
//! - 无 `Authorization` 头 → **放行**（由 `Principal` 提取器决定是否 401，或由
//!   `OptionalPrincipal` 走公开路由）
//! - 有 token 但校验失败 → **直接 401**（fail-fast，避免无效请求进入业务）

use std::sync::Arc;

use axum::extract::Request;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::identity::Identity;
use crate::auth::{AuthService, jwt::JwtCodec};
use crate::errors::ErrorCode;

/// 解析 Bearer 令牌并注入 [`Identity`]。
///
/// 供 `ServiceBuilder` 使用：`from_fn(move |req, next| authenticate(auth.clone(), req, next))`。
pub async fn authenticate(auth: Arc<AuthService>, req: Request, next: Next) -> Response {
    let Some(header_value) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        // 无 Authorization 头：放行，交给提取器决定
        return next.run(req).await;
    };

    let Some(token) = JwtCodec::bearer_token(&header_value) else {
        return crate::errors::AppError::biz(
            ErrorCode::UNAUTHENTICATED,
            "malformed Authorization header",
        )
        .into_response();
    };

    // 限流层（`per_key`）为了拿到可信主体已经验过同一个令牌：直接复用，
    // 避免同一请求验两次。扩展只由框架自身的层写入，客户端无法注入；
    // 验签失败时根本不会写入，故不影响下面的 fail-fast 语义。
    let claims = match req
        .extensions()
        .get::<crate::auth::VerifiedClaims>()
        .map(|verified| verified.0.clone())
    {
        Some(claims) => claims,
        None => match auth.jwt().verify_access(token) {
            Ok(claims) => claims,
            // 校验失败：fail-fast 直接返回统一错误体
            Err(err) => return err.into_response(),
        },
    };

    // jti 黑名单（feature revocation）：登出 / 吊销后的令牌立即失效
    #[cfg(feature = "revocation")]
    if let Some(store) = auth.revocation() {
        match store.is_revoked(&claims.jti).await {
            Ok(true) => {
                return crate::errors::AppError::biz(ErrorCode::UNAUTHENTICATED, "令牌已吊销")
                    .into_response();
            }
            Ok(false) => {}
            Err(error) => {
                if store.fail_closed() {
                    tracing::error!(%error, "吊销检查失败，fail-closed：拒绝请求");
                    return crate::errors::AppError::biz(
                        ErrorCode::SERVICE_UNAVAILABLE,
                        "认证服务暂不可用",
                    )
                    .into_response();
                }
                tracing::warn!(%error, "吊销检查失败，按未吊销处理（fail-open）");
            }
        }
    }

    let mut req = req;
    req.extensions_mut().insert(Identity::jwt(
        claims.principal(),
        crate::utils::now_unix_secs(),
    ));
    next.run(req).await
}
