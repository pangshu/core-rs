//! Prometheus 指标（feature = "metrics"，文档 三·8）：
//! QPS / 延迟 / 错误率经请求计数与耗时直方图暴露 `/metrics`；
//! 关闭 feature 时零开销。

use std::time::Instant; // 引入单调时钟，用于测量请求耗时

use axum::extract::Request; // 引入请求类型（中间件入参）
use axum::middleware::Next; // 引入下游处理链
use axum::response::Response; // 引入响应类型
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle}; // 引入 Prometheus 记录器与渲染句柄

/// 安装全局 Prometheus recorder（进程内只能安装一次；重复调用返回 None）
pub fn init() -> Option<PrometheusHandle> { // 安装全局指标记录器
    PrometheusBuilder::new().install_recorder().ok() // 安装成功返回句柄，重复安装返回 None
}

/// 请求指标中间件：
/// `core_rs_http_requests_total{method,path,status}` /
/// `core_rs_http_request_duration_seconds{method,path}`
pub async fn track(req: Request, next: Next) -> Response { // 统计请求数与耗时
    let start = Instant::now(); // 记录开始时刻
    let method = req.method().clone(); // 克隆请求方法（后续要移动 req）
    // MatchedPath 在路由匹配后才存在（路由树内层）；未匹配（404）时**必须**归一化：
    // path 是 Prometheus label，按值去重存储，用户可控的原始路径会制造无上界时间序列
    let path = req // 从请求中取路由模板
        .extensions() // 访问扩展
        .get::<axum::extract::MatchedPath>() // 取已匹配路由
        .map(|p| p.as_str().to_string()) // 命中则用路由模板作为标签
        .unwrap_or_else(|| "__unmatched__".to_string()); // 未匹配统一归一到固定标签，避免基数爆炸

    let resp = next.run(req).await; // 继续执行下游并取得响应

    let status = resp.status().as_u16().to_string(); // 取响应状态码作为标签
    metrics::counter!( // 请求总数计数器
        "core_rs_http_requests_total", // 指标名
        "method" => method.to_string(), // 方法标签
        "path" => path.clone(), // 路由标签
        "status" => status, // 状态码标签
    )
    .increment(1); // 计数 +1
    metrics::histogram!( // 请求耗时直方图
        "core_rs_http_request_duration_seconds", // 指标名
        "method" => method.to_string(), // 方法标签
        "path" => path, // 路由标签
    )
    .record(start.elapsed().as_secs_f64()); // 记录本次耗时（秒）

    resp // 原样返回响应
}

/// /metrics 响应体渲染
pub fn render(handle: &PrometheusHandle) -> String { // 渲染 Prometheus 文本格式
    handle.render() // 由句柄渲染当前指标快照
}
