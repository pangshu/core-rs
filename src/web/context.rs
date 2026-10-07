//! [`RequestContext`]：一次请求的上下文（request_id / trace_id / locale / 身份），
//! 由中间件链逐段填充，经 extension 传递；handler 里声明 `ctx: RequestContext` 提取。
//!
//! - `request_id`：**允许前端带入**（`X-Request-Id`，缺失则服务端生成），跨层/跨系统关联；
//! - `trace_id`：**始终由服务端生成、不信任外部传入**，服务端内部链路追踪
//!   （文档 三·8；otel feature 下合法 W3C traceparent 的 trace-id 段会被采纳）。

use axum::extract::FromRequestParts; // 引入「从请求部件提取」trait
use axum::http::request::Parts; // 引入请求部件类型，提取器签名入参

/// 请求上下文
#[derive(Debug, Clone)] // 派生调试/克隆，便于跨中间件复制
pub struct RequestContext { // 定义单次请求上下文结构体
    pub request_id: String, // 请求 id（可来自前端，缺失则服务端生成）
    pub trace_id: String, // 链路追踪 id（始终服务端生成）
    /// Locale 协商结果（feature = "i18n" 时由 locale 中间件填充；缺省为默认语言）
    #[cfg(feature = "i18n")] // 仅在开启 i18n 时才有该字段
    pub locale: crate::i18n::locale::Locale, // 协商得到的语言区域
    /// 认证身份（auth 中间件填充；匿名请求为 None）
    pub identity: Option<crate::auth::Identity>, // 已认证身份，匿名为 None
}

impl RequestContext { // 为请求上下文实现构造方法
    pub(crate) fn new(request_id: String, trace_id: String) -> Self { // 以双 id 构造上下文
        Self { // 构造自身实例
            request_id, // 存入请求 id
            trace_id, // 存入追踪 id
            #[cfg(feature = "i18n")] // 仅在开启 i18n 时初始化该字段
            locale: crate::i18n::locale::Locale::default(), // 先置默认语言，待 locale 中间件覆盖
            identity: None, // 初始未认证
        }
    }
}

impl<S> FromRequestParts<S> for RequestContext // 让 RequestContext 可作为提取器参数
where
    S: Send + Sync, // 要求状态类型线程安全
{
    type Rejection = std::convert::Infallible; // 该提取器永不失败

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> { // 从扩展取上下文
        // 中间件缺失（裸 Router 直接测试）时退化为现场生成，保证提取器永不失败
        Ok(parts.extensions.get::<RequestContext>().cloned().unwrap_or_else(|| { // 取已注入上下文，缺失则新建
            RequestContext::new(crate::utils::new_id(), crate::utils::new_id()) // 现场生成两个新 id 兜底
        }))
    }
}
