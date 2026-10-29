//! `AppError` 的 HTTP 响应映射。
//!
//! 将 [`AppError`] 转换为统一响应体 `{ code, message, details?, data, trace_id }`，
//! 其中 HTTP 状态码由 [`ErrorCode::status`] 唯一推导（一码一态）。
//!
//! 三条安全约束（见 `docs/09` §4）：
//!
//! 1. **内部细节只进日志**：[`AppError::Internal`] 对外固定文案；
//! 2. **`details` 仅在 4xx 输出**，并受体积 / 深度上限约束（防响应放大）；
//!    该闸门在 [`ApiResponse::into_response`] 里统一收口（业务直接构造
//!    `ApiResponse` 时同样生效），本文件只负责把 `details` 递过去；
//! 3. **安全类拒绝留审计痕迹**：401 / 403 / 429 打一条 warn，否则撞库、
//!    越权探测在日志里完全不可见。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::errors::app_error::AppError;
use crate::errors::code::ErrorCode;
use crate::http::response::ApiResponse;
use crate::utils::string::log_safe;

/// 单条日志中错误文本的最大字符数。
const MAX_LOG_CHARS: usize = 512;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, message, details) = match self {
            AppError::Biz {
                code,
                message,
                details,
            } => (code, message, details),
            AppError::Internal(err) => {
                // 内部细节只进日志，对外仅返回固定文案。
                log_internal(&err);
                (
                    ErrorCode::INTERNAL,
                    ErrorCode::INTERNAL.default_message().to_string(),
                    None,
                )
            }
        };

        let status = code.status();
        let trace_id = crate::telemetry::current_trace_id();

        // 认证 / 授权 / 限流拒绝留审计痕迹。`message` 本身是对外文案，
        // 但仍做单行化：其中的换行可伪造日志行（日志注入）。
        if is_security_rejection(status) {
            tracing::warn!(
                code = code.as_i32(),
                status = status.as_u16(),
                trace_id = %trace_id,
                message = %log_safe(&message, MAX_LOG_CHARS),
                "request rejected"
            );
        }

        // `details` 的闸门（仅 4xx + 深度 / 体积上限）在 `ApiResponse::into_response`
        // 里统一收口——那是所有响应（`AppError` 与业务直构 `ApiResponse`）的共同出口。
        ApiResponse::<()>::err_with_details(code, message, details, trace_id).into_response()
    }
}

/// 该状态码是否属于需要留审计痕迹的安全类拒绝。
fn is_security_rejection(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    )
}

/// 记录内部错误。
///
/// 不用 `?err`（`anyhow::Error` 的 `Debug` 会打印完整 context 链、多行文本，
/// 甚至 backtrace）：既可能把凭证 / PII 写进日志，也会破坏单行日志结构。
/// 这里按「首层 + 链路数组」输出，并对每层做控制字符替换与截断。
fn log_internal(err: &anyhow::Error) {
    let chain: Vec<String> = err
        .chain()
        .map(|cause| log_safe(&cause.to_string(), MAX_LOG_CHARS))
        .collect();
    let head = chain.first().cloned().unwrap_or_default();
    tracing::error!(
        error = %head,
        error_chain = ?chain,
        "internal error"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::response::MAX_DETAILS_BYTES;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn should_map_not_found_to_404_with_unified_body() {
        // trace_id 取自请求作用域；作用域外是 `unknown`（见 telemetry::layer）
        let json = crate::telemetry::scope(crate::telemetry::TraceId::new(), async {
            let response = AppError::not_found("user 1 not found").into_response();
            assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        })
        .await;
        assert_eq!(json["code"], 30001);
        assert_eq!(json["message"], "user 1 not found");
        assert!(json["data"].is_null());
        assert_eq!(json["trace_id"].as_str().map(str::len), Some(32));
        assert!(
            json.get("details").is_none(),
            "无明细时不应出现 details 字段"
        );
    }

    #[tokio::test]
    async fn should_not_leak_internal_details_when_internal_error() {
        let response = AppError::internal(anyhow::anyhow!("secret db dsn")).into_response();
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["code"], 50001);
        assert_eq!(json["message"], "服务内部错误");
        assert!(!String::from_utf8_lossy(&bytes).contains("secret db dsn"));
    }

    #[tokio::test]
    async fn should_return_details_when_client_error() {
        let response = AppError::biz_with_details(
            ErrorCode::INVALID_PARAM,
            "参数校验失败",
            serde_json::json!({ "email": ["格式错误"] }),
        )
        .into_response();
        assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["details"]["email"][0], "格式错误");
    }

    #[tokio::test]
    async fn should_drop_details_when_server_error() {
        // 5xx 明细可能含内部结构：即使调用方塞了 details 也必须丢弃
        let response = AppError::biz_with_details(
            ErrorCode::DB_ERROR,
            "数据访问失败",
            serde_json::json!({ "table": "users", "sql": "SELECT * FROM users" }),
        )
        .into_response();
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("details").is_none(), "5xx 不得返回 details");
        assert!(!String::from_utf8_lossy(&bytes).contains("SELECT * FROM users"));
    }

    #[tokio::test]
    async fn should_drop_details_when_too_large_or_too_deep() {
        let huge = serde_json::json!({ "field": "x".repeat(MAX_DETAILS_BYTES + 1) });
        let deep = serde_json::json!({ "a": { "b": { "c": { "d": 1 } } } });

        for details in [huge, deep] {
            let response = AppError::biz_with_details(ErrorCode::INVALID_PARAM, "bad", details)
                .into_response();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                json.get("details").is_none(),
                "超限明细必须丢弃，实际：{}",
                json
            );
        }
    }

    #[tokio::test]
    async fn should_map_ok_code_to_internal_when_used_as_error() {
        // 成功码不能承载错误：`AppError::biz(OK, ..)` 归一到 50001，不能返回 200
        let response = AppError::biz(ErrorCode::OK, "boom").into_response();
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}
