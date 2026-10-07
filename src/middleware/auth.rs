//! 认证中间件：调用 `[auth]` 装配的认证链，把 [`Identity`] 注入 extension 并
//! 填充 `RequestContext.identity`。
//!
//! 认证**不拦截**：匿名请求照常放行，登录态要求由 `CurrentUser` 提取器
//! （401）与 `authz` 层（403）承担；或开启 `[auth] require_auth_by_default`
//! 后由 [`require_identity`] 兜底（P1-9）。凭据无效（如 token 过期）则立即 401。
//!
//! 依赖锚点：经请求 extension 读取 `CoreState`（`App::serve` 挂在最外层；
//! 裸模式自组装时同样由框架必需件保证存在，缺扩展时 500 fail-closed）。

use axum::extract::{Extension, Request}; // 引入扩展提取器与请求体类型
use axum::middleware::Next; // 引入 Next，用于把请求交给下游中间件
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型

use crate::state::CoreState; // 引入框架核心状态（依赖锚点）
use crate::traits::HasAuth; // 引入状态能力 trait：取认证器

/// 受保护路由的兜底：要求请求已携带 Identity（由 auth 中间件在外侧注入），
/// 无凭据 401。默认模式下 `require_auth_by_default = true` 时由 `App::serve`
/// 自动挂在 `.mount()` 的路由子树上；裸模式自组装用 [`require_identity_layer`]。
///
/// **fail-closed**：本层只查 extension、自己不做认证——必须把 auth 中间件挂在
/// 本层**外侧**，否则无人注入 Identity，受保护路由一律 401（而绝不会静默公开）。
pub async fn require_identity(req: Request, next: Next) -> Response { // 受保护路由兜底：要求已注入 Identity
    if req.extensions().get::<crate::auth::Identity>().is_some() { // 扩展中已有身份
        next.run(req).await // 放行到下游
    } else { // 无身份
        crate::error::AppError::unauthorized("authentication required").into_response() // 返回 401
    }
}

pub async fn handle(Extension(core): Extension<CoreState>, req: Request, next: Next) -> Response { // 认证中间件入口：认证但不拦截
    let Some(authn) = core.authn() else { // 取认证链
        return next.run(req).await; // 未启用认证则直接放行
    };

    // authenticate 只需要 Parts（headers/extensions），拆包借用后再拼回
    let (mut parts, body) = req.into_parts(); // 拆解请求为头部与 body
    let identity = match authn.authenticate(&parts).await { // 用请求头部执行认证
        Ok(Some(identity)) => Some(identity), // 认证成功：拿到身份
        // 凭据无效（过期 / 签名错）：直接 401，不再往后走
        Ok(None) => None, // 匿名：无身份
        Err(e) => return e.into_response(), // 凭据无效：立即返回错误响应
    };
    if let Some(identity) = &identity { // 若认证出身份
        parts.extensions.insert(identity.clone()); // 克隆并注入 extension 供下游使用
    }
    if let Some(ctx) = parts.extensions.get_mut::<crate::web::RequestContext>() { // 若存在请求上下文
        ctx.identity = identity; // 填充上下文中的身份字段
    }
    next.run(Request::from_parts(parts, body)).await // 重新拼回请求并交给下游
}

/// 自组装用层（裸模式）：认证但不拦截，注入 Identity。必须挂在
/// [`require_identity_layer`] / `CurrentUser` 的**外侧**，否则无人注入身份。
pub fn layer() -> super::BoxedLayer { // 返回装箱的认证层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}

/// 自组装用层（裸模式）：受保护子树的登录态兜底（无 Identity 401）。
/// 须挂在 [`layer`] 的内侧。
pub fn require_identity_layer() -> super::BoxedLayer { // 返回装箱的登录态兜底层
    super::BoxedLayer::new(axum::middleware::from_fn(require_identity)) // 装箱屏蔽具体层类型
}
