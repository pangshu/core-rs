//! 认证中间件：调用 `[auth]` 装配的认证链，把 [`Identity`] 注入 extension 并
//! 填充 `RequestContext.identity`。
//!
//! 认证**不拦截**：匿名请求照常放行，登录态要求由 `CurrentUser` 提取器
//! （401）与 `authz` 层（403）承担。凭据无效（如 token 过期）则立即 401。

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::traits::HasAuth;

pub(crate) async fn handle<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasAuth + Send + Sync + 'static,
{
    let Some(authn) = state.authn() else {
        return next.run(req).await;
    };

    // authenticate 只需要 Parts（headers/extensions），拆包借用后再拼回
    let (mut parts, body) = req.into_parts();
    let identity = match authn.authenticate(&parts).await {
        Ok(Some(identity)) => Some(identity),
        // 凭据无效（过期 / 签名错）：直接 401，不再往后走
        Ok(None) => None,
        Err(e) => return e.into_response(),
    };
    if let Some(identity) = &identity {
        parts.extensions.insert(identity.clone());
    }
    if let Some(ctx) = parts.extensions.get_mut::<crate::web::RequestContext>() {
        ctx.identity = identity;
    }
    next.run(Request::from_parts(parts, body)).await
}
