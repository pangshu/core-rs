//! Locale 协商（feature = "i18n"）：解析 `Accept-Language` / 用户偏好 →
//! 注入 `RequestContext.locale`（文档 三·20）。回退链与支持语言由 `[i18n]` 配置。
//!
//! 挂载位置必须在 request_id **之后**——RequestContext 此时才存在；
//! i18n feature 关闭时本模块编译为 no-op 直通。默认模式下由
//! `web/router.rs::base_layers` 固定在推荐位次；裸模式自组装用 [`layer`]。
//!
//! 依赖锚点：经请求 extension 读取 `CoreState`（`App::serve` 挂在最外层；
//! 裸模式自组装时同样由框架必需件保证存在，缺扩展时 500 fail-closed）。

use axum::extract::{Extension, Request}; // 引入扩展提取器与请求类型
use axum::middleware::Next; // 引入 Next，用于把请求交给下游中间件
use axum::response::Response; // 引入响应类型

use crate::state::CoreState; // 引入框架核心状态（依赖锚点）
#[cfg(feature = "i18n")] // 仅 i18n 开启版实现读取配置，直通版无此依赖
use crate::traits::HasConfig; // 引入「能提供配置句柄」的能力 trait

#[cfg(feature = "i18n")] // 仅在开启 i18n 时编译下面这版实现
pub(crate) async fn handle(Extension(core): Extension<CoreState>, mut req: Request, next: Next) -> Response { // 带状态的 locale 中间件主体
    let i18n = &core.config().load().i18n; // 读取 i18n 配置（无锁快照，支持热更新）
    if i18n.enabled { // 仅在启用多语言时协商
        let header = req // 取 Accept-Language 请求头
            .headers()
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let locale = crate::i18n::negotiate(header, i18n); // 按回退链协商出目标语言
        if let Some(ctx) = req.extensions_mut().get_mut::<crate::web::RequestContext>() { // 取可变请求上下文
            ctx.locale = locale; // 写入协商结果
        }
    }
    next.run(req).await // 继续下游处理
}

/// i18n 关闭时的直通实现：保持 base_layers 装配代码与 feature 无关
#[cfg(not(feature = "i18n"))] // 仅在未开启 i18n 时编译下面这版实现
pub(crate) async fn handle(Extension(_core): Extension<CoreState>, req: Request, next: Next) -> Response { // 直通版 locale 中间件
    next.run(req).await // 不做任何协商，直接放行
}

/// 自组装用层（裸模式）：Locale 协商。必须挂在 request_id **内侧**
/// （RequestContext 此时才存在）；生效与否由 `[i18n] enabled` 配置决定。
pub fn layer() -> super::BoxedLayer { // 返回装箱的 locale 层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}
