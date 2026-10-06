//! 统一错误 [`AppError`]：业务代码只管返回这个类型，框架负责映射为 HTTP 状态码
//! 和统一响应体 `{ code, message, data }`。内部错误细节只进日志，不外泄给客户端。
//!
//! 业务码分段约定（文档 三·7）：
//! - `0` 成功；`4xx/5xx` 与 HTTP 状态码对齐；`6401` 保留（token 过期需刷新）；
//! - `≥1000` 为应用自定义业务码段（[`AppError::Biz`]），HTTP 统一 422。

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

    /// token 签名合法但已过期：HTTP 仍为 401，响应体 `code = 6401`（保留码，
    /// 前端据此区分「需要刷新」与「未认证」）
    #[error("{0}")]
    TokenExpired(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    TooManyRequests(String),

    /// 应用自定义业务码（≥1000），HTTP 422；消息按 locale 翻译（feature = "i18n"）
    #[error("{1}")]
    Biz(i32, String),

    /// 参数校验失败（garde 报告，归一化为 字段路径 → 消息；进响应 data 字段）
    #[error("validation failed")]
    Validation(Vec<ValidationItem>),

    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),

    #[error("cache error: {0}")]
    Cache(#[from] CacheError),

    #[error("queue error: {0}")]
    Queue(#[from] crate::queue::QueueError),

    #[error("config error: {0}")]
    Config(#[from] config::ConfigError),

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

    pub fn too_many_requests(msg: impl Into<String>) -> Self {
        Self::TooManyRequests(msg.into())
    }

    pub fn biz(code: i32, msg: impl Into<String>) -> Self {
        debug_assert!(code >= 1000, "业务码约定 ≥1000，避免与 HTTP 映射码冲突");
        Self::Biz(code, msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// HTTP 状态码与响应体 code（公开给业务/测试做错误码断言）
    pub fn status_and_code(&self) -> (StatusCode, i32) {
        use AppError::*;
        match self {
            BadRequest(_) | Validation(_) => (StatusCode::BAD_REQUEST, 400),
            Unauthorized(_) => (StatusCode::UNAUTHORIZED, 401),
            // 过期与未认证 HTTP 同为 401，靠响应体 code 6401 区分
            TokenExpired(_) => (StatusCode::UNAUTHORIZED, 6401),
            Forbidden(_) => (StatusCode::FORBIDDEN, 403),
            NotFound(_) => (StatusCode::NOT_FOUND, 404),
            TooManyRequests(_) => (StatusCode::TOO_MANY_REQUESTS, 429),
            // 业务码段：HTTP 422，body.code 透传应用自定义码
            Biz(code, _) => (StatusCode::UNPROCESSABLE_ENTITY, *code),
            Db(_) | Cache(_) | Config(_) | Io(_) | Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, 500)
            }
            Queue(_) => (StatusCode::INTERNAL_SERVER_ERROR, 500),
        }
    }

    /// 5xx 细节只记日志，对外统一话术；4xx 语义原样输出
    pub(crate) fn client_message(&self) -> String {
        let (status, _) = self.status_and_code();
        if status.is_server_error() {
            tracing::error!(error = %self, "internal error");
            "internal server error".to_string()
        } else {
            self.to_string()
        }
    }
}

impl From<garde::Report> for AppError {
    fn from(report: garde::Report) -> Self {
        AppError::Validation(ValidationItem::from_report(&report))
    }
}

/// 校验错误明细（字段路径 → 消息），用于响应体 data 字段
#[derive(Debug, Clone, serde::Serialize)]
pub struct ValidationItem {
    pub field: String,
    pub message: String,
}

impl ValidationItem {
    pub fn from_report(report: &garde::Report) -> Vec<Self> {
        report
            .iter()
            .map(|(path, err)| Self {
                field: path.to_string(),
                message: err.to_string(),
            })
            .collect()
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = self.client_message();
        let body: ApiResponse<serde_json::Value> = match &self {
            AppError::Validation(items) => ApiResponse {
                code,
                message: message.clone(),
                data: serde_json::to_value(items).unwrap_or(serde_json::Value::Null),
            },
            _ => ApiResponse {
                code,
                message,
                data: serde_json::Value::Null,
            },
        };
        (status, Json(body)).into_response()
    }
}
