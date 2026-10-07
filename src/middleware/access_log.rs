//! 访问日志：method / path / status / 耗时（在 trace span 内输出，自动携带
//! request_id / trace_id）。4xx 记 warn、5xx 记 error、其余 info。

use std::time::Instant; // 引入单调时钟，用于测量耗时

use axum::extract::Request; // 引入 axum 请求类型
use axum::middleware::Next; // 引入中间件链的下一个处理器
use axum::response::Response; // 引入响应类型

pub(crate) async fn handle(req: Request, next: Next) -> Response { // 访问日志中间件主体
    let start = Instant::now(); // 记录请求开始时刻
    let method = req.method().clone(); // 复制请求方法（后续 req 会被消费）
    let path = req.uri().path().to_string(); // 复制请求路径字符串
    // MatchedPath 在路由匹配后才存在（路由树内层），外层中间件取不到时退化为原始 path
    let matched = req // 尝试获取路由模板路径
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string());

    let res = next.run(req).await; // 调用下游处理请求并取得响应

    let status = res.status(); // 读取响应状态码
    let elapsed_ms = start.elapsed().as_millis() as u64; // 计算处理耗时（毫秒）
    let template = matched.as_deref().unwrap_or(&path); // 优先用路由模板，缺失则用原始路径
    if status.is_server_error() { // 5xx 服务端错误
        tracing::error!(method = %method, path = %template, status = status.as_u16(), elapsed_ms, "access"); // 记 error 级访问日志
    } else if status.is_client_error() { // 4xx 客户端错误
        tracing::warn!(method = %method, path = %template, status = status.as_u16(), elapsed_ms, "access"); // 记 warn 级访问日志
    } else { // 其余（2xx/3xx）正常
        tracing::info!(method = %method, path = %template, status = status.as_u16(), elapsed_ms, "access"); // 记 info 级访问日志
    }
    res // 原样返回响应
}

/// 自组装用层（裸模式）：访问日志。须挂在 request_id 内侧（日志自动携带双 id）。
pub fn layer() -> super::BoxedLayer { // 返回装箱的访问日志层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}
