//! 受保护路由的守卫层（feature `auth`）。
//!
//! 与 [`crate::auth::authenticate`] 的分工：
//!
//! - `authenticate`（全局栈，外层）：**有** `Authorization` 头时验签并注入
//!   [`Identity`](crate::auth::Identity)；没有则放行（因为公开路由也要走完整栈）。
//! - 本层（只挂在受保护路由组上，内层）：检查扩展里有没有 `Identity`，
//!   没有就 `20001`（401）。
//!
//! 这样"鉴权"就从"handler 签名里记得写 `Principal`"变成了"路由分组时就决定"，
//! 漏写参数不再等于公开接口。
//!
//! 层序说明：`build_router` 在 merge 之前对该子路由 `.layer()`，因此本层位于
//! 全局中间件栈**内侧**——`authenticate` 一定先跑，本层看到的是最终身份。

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::errors::AppError;

/// 要求请求已携带有效身份。
pub(crate) async fn require_identity(req: Request, next: Next) -> Response {
    if req.extensions().get::<crate::auth::Identity>().is_none() {
        tracing::warn!(
            method = %req.method(),
            path = %req.uri().path(),
            "受保护路由未携带有效身份"
        );
        return AppError::unauthenticated("authentication required").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;
    use crate::auth::Identity;
    use crate::auth::identity::Principal;

    fn app() -> Router {
        Router::new()
            .route("/guarded", get(|| async { "ok" }))
            .layer(from_fn(require_identity))
    }

    #[tokio::test]
    async fn should_reject_request_without_identity() {
        let response = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/guarded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn should_allow_request_with_identity() {
        let request = HttpRequest::builder()
            .uri("/guarded")
            .extension(Identity::jwt(Principal::new("u"), 0))
            .body(Body::empty())
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
