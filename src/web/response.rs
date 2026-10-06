//! 统一响应体 `{ code, message, data }`。成功 code 固定为 [`CODE_OK`]（0），
//! 失败路径由 [`crate::web::error::AppError`] 生成同构响应。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::web::error::AppError;

/// 业务成功码。错误响应的 code：HTTP 映射类等于状态码（400/401/403/404/429/500），
/// 6401 保留给「token 过期需刷新」，≥1000 为应用自定义业务码段（文档 三·7）。
pub const CODE_OK: i32 = 0;

/// handler 统一返回类型：`?` 直接抛 [`AppError`]，成功值经 `ApiResponse::ok` 包装。
///
/// axum 对 `Result<T, E>`（两侧均实现 IntoResponse）有 blanket 实现，
/// 因此无需为该别名单独实现 IntoResponse。
pub type ApiResult<T> = Result<ApiResponse<T>, AppError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub code: i32,
    pub message: String,
    pub data: T,
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            code: CODE_OK,
            message: "ok".to_string(),
            data,
        }
    }

    pub fn with_msg(message: impl Into<String>, data: T) -> Self {
        Self {
            code: CODE_OK,
            message: message.into(),
            data,
        }
    }
}

impl ApiResponse<()> {
    pub fn error(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: (),
        }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(self)).into_response()
    }
}

/// 统一分页响应（与 [`crate::db::paginate`] 的 ORM 分页字段对齐）
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub records: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub size: u64,
    pub pages: u64,
}

impl<T> Page<T> {
    pub fn new(records: Vec<T>, total: u64, page: u64, size: u64) -> Self {
        let pages = if size == 0 { 0 } else { total.div_ceil(size) };
        Self {
            records,
            total,
            page,
            size,
            pages,
        }
    }
}
