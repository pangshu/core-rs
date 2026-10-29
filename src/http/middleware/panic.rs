//! panic 兜底中间件。
//!
//! 依赖 `panic = "unwind"`（见 `docs/18`）。捕获 handler panic，产出统一错误体。

use std::any::Any;

use axum::response::IntoResponse;
use tower_http::catch_panic::CatchPanicLayer;

use crate::errors::AppError;

/// 自定义 panic 处理器类型。
pub(crate) type PanicFn = fn(Box<dyn Any + Send + 'static>) -> axum::response::Response;

/// 构造 panic 兜底 Layer。
pub(crate) fn layer() -> CatchPanicLayer<PanicFn> {
    CatchPanicLayer::custom(on_panic as PanicFn)
}

fn on_panic(err: Box<dyn Any + Send + 'static>) -> axum::response::Response {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "unknown panic".to_string()
    };
    // panic 载荷常常内嵌用户输入（`unwrap()` / 索引越界的信息里带字段值），
    // 原样进日志会破坏单行结构（日志注入）并可能写出敏感数据；统一过 `log_safe`。
    tracing::error!(
        panic = %crate::utils::string::log_safe(&detail, 512),
        "handler panicked"
    );
    AppError::internal(anyhow::anyhow!("handler panicked")).into_response()
}
