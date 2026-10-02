//! 统一响应体 `{ code, msg, data }`。成功 code 固定为 [`CODE_OK`]（0），
//! 失败路径由 [`crate::error::AppError`] 生成同构响应。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::error::AppError;

/// 业务成功码。错误响应的 code 取对应 HTTP 状态码。
pub const CODE_OK: i32 = 0;

/// handler 统一返回类型：`?` 直接抛 [`AppError`]，成功值经 `ApiResponse::ok` 包装。
///
/// axum 对 `Result<T, E>`（两侧均实现 IntoResponse）有 blanket 实现，
/// 因此无需为该别名单独实现 IntoResponse。
pub type ApiResult<T> = Result<ApiResponse<T>, AppError>;

#[derive(Debug, Clone, Serialize)]
pub struct ApiResponse<T> {
    pub code: i32,
    pub msg: String,
    pub data: T,
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            code: CODE_OK,
            msg: "ok".to_string(),
            data,
        }
    }

    pub fn with_msg(msg: impl Into<String>, data: T) -> Self {
        Self {
            code: CODE_OK,
            msg: msg.into(),
            data,
        }
    }
}

impl ApiResponse<()> {
    pub fn error(code: i32, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
            data: (),
        }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(self)).into_response()
    }
}
