//! 双 id 中间件（文档 三·8）：
//!
//! - `request_id`：**允许前端带入**（`X-Request-Id`，缺失则服务端生成），
//!   用于跨层 / 跨系统关联——网关、前端、后端对齐同一次调用；
//! - `trace_id`：**始终由服务端生成、不信任外部传入**（客户端传什么都被忽略），
//!   用于服务端内部链路追踪，防追踪数据被伪造 / 污染。
//!   otel feature 下，合法 W3C `traceparent` 头的 trace-id 段会被采纳，
//!   使本地日志与导出的 OTel trace 可直接互查。
//!
//! 两个 id 都注入 `RequestContext` 并回传响应头，日志默认同时携带。

use axum::extract::Request;
use axum::http::{header::HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const TRACE_ID_HEADER: &str = "x-trace-id";
#[cfg(feature = "otel")]
const TRACEPARENT_HEADER: &str = "traceparent";

pub(crate) fn header_value(req: &Request) -> String {
    req.headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn trace_header_value(req: &Request) -> String {
    req.headers()
        .get(TRACE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn handle(mut req: Request, next: Next) -> Response {
    let request_id = match req.headers().get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok()) {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => crate::utils::new_id(),
    };
    // trace_id 服务端生成（不信任外部）；otel 部署采纳合法 traceparent 的 trace-id 段
    let trace_id = trace_source(&req).unwrap_or_else(crate::utils::new_id);

    let name: HeaderName = REQUEST_ID_HEADER.parse().expect("static header name");
    let tname: HeaderName = TRACE_ID_HEADER.parse().expect("static header name");
    let resp_name = name.clone();
    let resp_tname = tname.clone();
    req.headers_mut().insert(
        name,
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("-")),
    );
    req.headers_mut().insert(
        tname,
        HeaderValue::from_str(&trace_id).unwrap_or_else(|_| HeaderValue::from_static("-")),
    );

    req.extensions_mut().insert(crate::web::RequestContext::new(
        request_id.clone(),
        trace_id.clone(),
    ));

    let mut res = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        res.headers_mut().insert(resp_name, v);
    }
    if let Ok(v) = HeaderValue::from_str(&trace_id) {
        res.headers_mut().insert(resp_tname, v);
    }
    res
}

fn trace_source(req: &Request) -> Option<String> {
    #[cfg(feature = "otel")]
    {
        if let Some(tp) = req
            .headers()
            .get(TRACEPARENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_traceparent_trace_id)
        {
            return Some(tp);
        }
    }
    let _ = req;
    None
}

/// 从 W3C traceparent（`<version>-<32位traceid>-<16位spanid>-<flags>`）提取
/// trace-id 段；全零 trace-id 按规范视为无效
#[cfg(feature = "otel")]
fn parse_traceparent_trace_id(tp: &str) -> Option<String> {
    let seg: Vec<&str> = tp.split('-').collect();
    match seg.as_slice() {
        [_version, trace_id, _span_id, _flags]
            if trace_id.len() == 32
                && trace_id.chars().all(|c| c.is_ascii_hexdigit())
                && trace_id.chars().any(|c| c != '0') =>
        {
            Some(trace_id.to_string())
        }
        _ => None,
    }
}

#[cfg(all(test, feature = "otel"))]
mod tests {
    use super::*;

    #[test]
    fn traceparent_parsing() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            parse_traceparent_trace_id(valid).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        // 全零 trace-id 按规范无效
        assert_eq!(
            parse_traceparent_trace_id("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            None
        );
        assert_eq!(parse_traceparent_trace_id("00-short-00f067aa0ba902b7-01"), None);
        assert_eq!(parse_traceparent_trace_id("garbage"), None);
    }
}
