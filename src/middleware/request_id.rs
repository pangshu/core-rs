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

use axum::extract::Request; // 引入 axum 请求类型
use axum::http::{header::HeaderName, HeaderValue}; // 引入请求头名与值类型
use axum::middleware::Next; // 引入中间件链的下一个处理器
use axum::response::Response; // 引入响应类型

pub const REQUEST_ID_HEADER: &str = "x-request-id"; // 请求 id 头名常量
pub const TRACE_ID_HEADER: &str = "x-trace-id"; // 追踪 id 头名常量
#[cfg(feature = "otel")] // 仅在开启 otel 时编译下面常量
const TRACEPARENT_HEADER: &str = "traceparent"; // W3C 追踪上下文头名常量

pub(crate) fn header_value(req: &Request) -> String { // 读取请求头中的 request_id（供 trace span 记录）
    req.headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn trace_header_value(req: &Request) -> String { // 读取请求头中的 trace_id（供 trace span 记录）
    req.headers()
        .get(TRACE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn handle(mut req: Request, next: Next) -> Response { // 双 id 中间件主体
    let request_id = match req.headers().get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok()) { // 尝试采用前端传入的 request_id
        Some(v) if !v.trim().is_empty() => v.trim().to_string(), // 非空则去空白后采用
        _ => crate::utils::new_id(), // 缺失或空白则服务端生成
    };
    // trace_id 服务端生成（不信任外部）；otel 部署采纳合法 traceparent 的 trace-id 段
    let trace_id = trace_source(&req).unwrap_or_else(crate::utils::new_id); // 取追踪来源，无则生成新 id

    let name: HeaderName = REQUEST_ID_HEADER.parse().expect("static header name"); // 解析请求 id 头名（静态常量必成功）
    let tname: HeaderName = TRACE_ID_HEADER.parse().expect("static header name"); // 解析追踪 id 头名（静态常量必成功）
    let resp_name = name.clone(); // 复制头名供响应阶段使用
    let resp_tname = tname.clone(); // 复制追踪头名供响应阶段使用
    req.headers_mut().insert( // 把 request_id 写回请求头，供下游读取
        name,
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("-")), // 非法字符则退化为 "-"
    );
    req.headers_mut().insert( // 把 trace_id 写回请求头，供下游读取
        tname,
        HeaderValue::from_str(&trace_id).unwrap_or_else(|_| HeaderValue::from_static("-")), // 非法字符则退化为 "-"
    );

    req.extensions_mut().insert(crate::web::RequestContext::new( // 注入请求上下文，供提取器使用
        request_id.clone(), // 传入 request_id
        trace_id.clone(), // 传入 trace_id
    ));

    let mut res = next.run(req).await; // 调用下游并取得响应
    if let Ok(v) = HeaderValue::from_str(&request_id) { // 若 request_id 可转头值
        res.headers_mut().insert(resp_name, v); // 回传 x-request-id 响应头
    }
    if let Ok(v) = HeaderValue::from_str(&trace_id) { // 若 trace_id 可转头值
        res.headers_mut().insert(resp_tname, v); // 回传 x-trace-id 响应头
    }
    res // 返回响应
}

/// 自组装用层（裸模式）：双 id 生成/透传。框架必需件之一（`App::bare` 模式下
/// 由框架自动挂载）；自组装时须挂在 locale/trace/access_log 的**外侧**。
pub fn layer() -> super::BoxedLayer { // 返回装箱的双 id 层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}

fn trace_source(req: &Request) -> Option<String> { // 解析追踪来源（otel 下采纳合法 traceparent）
    #[cfg(feature = "otel")] // 仅在开启 otel 时编译下面分支
    {
        if let Some(tp) = req // 尝试从 traceparent 头提取 trace-id
            .headers()
            .get(TRACEPARENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_traceparent_trace_id)
        {
            return Some(tp); // 解析成功则直接采用
        }
    }
    let _ = req; // 未开启 otel 时消解未使用参数告警
    None // 无可用外部追踪来源
}

/// 从 W3C traceparent（`<version>-<32位traceid>-<16位spanid>-<flags>`）提取
/// trace-id 段；全零 trace-id 按规范视为无效
#[cfg(feature = "otel")] // 仅在开启 otel 时编译下面函数
fn parse_traceparent_trace_id(tp: &str) -> Option<String> { // 解析 traceparent 并校验 trace-id 段
    let seg: Vec<&str> = tp.split('-').collect(); // 按 "-" 拆成四段
    match seg.as_slice() { // 匹配四段结构
        [_version, trace_id, _span_id, _flags] // 恰为四段
            if trace_id.len() == 32 // 且 trace-id 长度为 32
                && trace_id.chars().all(|c| c.is_ascii_hexdigit()) // 且全为十六进制字符
                && trace_id.chars().any(|c| c != '0') => // 且非全零（全零无效）
        {
            Some(trace_id.to_string()) // 校验通过则返回 trace-id 段
        }
        _ => None, // 结构或内容不合法则返回 None
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
