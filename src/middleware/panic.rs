//! 捕获 handler panic → 记日志 + 统一 500，避免连接被直接打断。
//! 层本身由 `tower-http` 的 `CatchPanicLayer` 承担（见 web/router.rs），
//! 本模块只提供统一响应体（与内部错误同款，细节不外泄）。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(crate) fn panic_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(crate::web::response::ApiResponse::error(
            500,
            "internal server error",
        )),
    )
        .into_response()
}
