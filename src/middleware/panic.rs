//! 捕获 handler panic → 记日志 + 统一 500，避免连接被直接打断。
//! 层本身由 `tower-http` 的 `CatchPanicLayer` 承担；本模块提供统一响应体
//! （与内部错误同款，细节不外泄）与层构造器。框架必需件之一：
//! 两种装配模式下 `App::serve` 都在最外圈挂载（裸模式自组装一般无需自挂）。

use axum::Router; // 引入路由类型（apply 的入参）
use axum::http::StatusCode; // 引入 HTTP 状态码类型
use axum::response::{IntoResponse, Response}; // 引入可转响应 trait 与响应类型

pub(crate) fn panic_response() -> Response { // 构造 panic 时的统一 500 响应
    (
        StatusCode::INTERNAL_SERVER_ERROR, // 状态码固定 500
        axum::Json(crate::web::response::ApiResponse::error( // 统一错误响应体
            500, // 业务码 500（与 HTTP 对齐）
            "internal server error", // 固定话术，不外泄 panic 细节
        )),
    )
        .into_response() // 元组组合为 HTTP 响应
}

/// panic 捕获层（handler panic 时客户端拿到统一 500 而不是连接重置）。
/// `App::serve` 两种模式下都会自动挂载；自组装 Router 时建议置于最外。
pub fn layer<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> { // 把 panic 捕获层挂到路由上
    router.layer(tower_http::catch_panic::CatchPanicLayer::custom(|_panic: Box<dyn std::any::Any + Send>| { // 自定义 panic 处理
        tracing::error!("handler panicked"); // 记录 handler panic 日志
        panic_response() // 返回统一 500 响应体
    }))
}
