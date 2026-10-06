//! [`RequestContext`]：一次请求的上下文（request_id / trace_id / locale / 身份），
//! 由中间件链逐段填充，经 extension 传递；handler 里声明 `ctx: RequestContext` 提取。
//!
//! - `request_id`：**允许前端带入**（`X-Request-Id`，缺失则服务端生成），跨层/跨系统关联；
//! - `trace_id`：**始终由服务端生成、不信任外部传入**，服务端内部链路追踪
//!   （文档 三·8；otel feature 下合法 W3C traceparent 的 trace-id 段会被采纳）。

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

/// 请求上下文
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub trace_id: String,
    /// Locale 协商结果（feature = "i18n" 时由 locale 中间件填充；缺省为默认语言）
    #[cfg(feature = "i18n")]
    pub locale: crate::i18n::locale::Locale,
    /// 认证身份（auth 中间件填充；匿名请求为 None）
    pub identity: Option<crate::auth::Identity>,
}

impl RequestContext {
    pub(crate) fn new(request_id: String, trace_id: String) -> Self {
        Self {
            request_id,
            trace_id,
            #[cfg(feature = "i18n")]
            locale: crate::i18n::locale::Locale::default(),
            identity: None,
        }
    }
}

impl<S> FromRequestParts<S> for RequestContext
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // 中间件缺失（裸 Router 直接测试）时退化为现场生成，保证提取器永不失败
        Ok(parts.extensions.get::<RequestContext>().cloned().unwrap_or_else(|| {
            RequestContext::new(crate::utils::new_id(), crate::utils::new_id())
        }))
    }
}
