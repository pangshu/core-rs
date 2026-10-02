//! 统一错误 [`AppError`]：业务代码只管返回这个类型，框架负责映射为 HTTP 状态码
//! 和统一响应体 `{ code, msg, data }`。内部错误细节只进日志，不外泄给客户端。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::cache::CacheError;
use crate::web::response::ApiResponse;

/// 应用层返回值别名
pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Unauthorized(String),

    /// token 签名合法但已过期：HTTP 仍为 401，响应体 `code = 6401`（自定义码，
    /// 前端据此区分「需要刷新」与「未认证」，需引导客户端调刷新接口换新 token）
    #[error("{0}")]
    TokenExpired(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("validation failed: {0}")]
    Validation(#[from] validator::ValidationErrors),

    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),

    #[error("cache error: {0}")]
    Cache(#[from] CacheError),

    #[cfg(feature = "queue")]
    #[error("queue error: {0}")]
    Queue(#[from] crate::queue::QueueError),

    #[error("config error: {0}")]
    Config(#[from] config::ConfigError),

    #[error("app config section error: {0}")]
    AppSection(#[from] crate::config::AppSectionError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Internal(String),
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// HTTP 状态码与响应体 code（公开给业务/测试做错误码断言；
    /// TokenExpired → `(401, 6401)`，其余见各变体）
    pub fn status_and_code(&self) -> (StatusCode, i32) {
        use AppError::*;
        match self {
            BadRequest(_) | Validation(_) => (StatusCode::BAD_REQUEST, 400),
            Unauthorized(_) => (StatusCode::UNAUTHORIZED, 401),
            // 过期与未认证 HTTP 同为 401，靠响应体 code 6401 区分
            TokenExpired(_) => (StatusCode::UNAUTHORIZED, 6401),
            Forbidden(_) => (StatusCode::FORBIDDEN, 403),
            NotFound(_) => (StatusCode::NOT_FOUND, 404),
            Db(_) | Cache(_) | Config(_) | AppSection(_) | Io(_) | Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, 500)
            }
            #[cfg(feature = "queue")]
            Queue(_) => (StatusCode::INTERNAL_SERVER_ERROR, 500),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();

        let msg = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // 内部错误细节只记日志，对外统一话术
            tracing::error!(error = %self, "internal error");
            "internal server error".to_string()
        } else {
            self.to_string()
        };

        (status, Json(ApiResponse::error(code, msg))).into_response()
    }
}
