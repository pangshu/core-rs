//! 请求上下文。

use crate::telemetry::TraceId;

/// 单个请求的上下文，便于在 handler / 日志中携带一致信息。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RequestContext {
    /// 请求方法。
    pub method: http::Method,
    /// 请求路径。
    pub path: String,
    /// 链路 ID。
    pub trace_id: TraceId,
}

impl RequestContext {
    /// 由方法、路径构造上下文（自动生成 trace-id）。
    #[must_use]
    pub fn new(method: http::Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            trace_id: TraceId::new(),
        }
    }
}
