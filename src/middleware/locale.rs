//! Locale 协商（feature = "i18n"）：解析 `Accept-Language` / 用户偏好 →
//! 注入 `RequestContext.locale`（文档 三·20）。回退链与支持语言由 `[i18n]` 配置。
//!
//! 挂载位置由 `web/router.rs::base_layers` 固定在 request_id **之后**——
//! RequestContext 此时才存在；i18n feature 关闭时本模块编译为 no-op 直通。

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::traits::HasConfig;

#[cfg(feature = "i18n")]
pub(crate) async fn handle<S>(State(state): State<S>, mut req: Request, next: Next) -> Response
where
    S: HasConfig + Send + Sync + 'static,
{
    let i18n = &state.config().load().i18n;
    if i18n.enabled {
        let header = req
            .headers()
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let locale = crate::i18n::negotiate(header, i18n);
        if let Some(ctx) = req.extensions_mut().get_mut::<crate::web::RequestContext>() {
            ctx.locale = locale;
        }
    }
    next.run(req).await
}

/// i18n 关闭时的直通实现：保持 base_layers 装配代码与 feature 无关
#[cfg(not(feature = "i18n"))]
pub(crate) async fn handle<S>(State(_state): State<S>, req: Request, next: Next) -> Response
where
    S: HasConfig + Send + Sync + 'static,
{
    next.run(req).await
}
