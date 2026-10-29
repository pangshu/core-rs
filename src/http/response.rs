//! 统一响应体。
//!
//! 契约见 `docs/09-error-and-response-spec.md` §1：
//! `{ code, message, data, trace_id }`（`details` 为可选的第 5 个字段，
//! 仅 4xx 且确有字段级明细时出现）；`data` 为 `null` 而非省略。
//!
//! **`details` 的闸门在本文件的 [`ApiResponse::into_response`]**，而不是在
//! `AppError` 的响应路径里：`ApiResponse` 自己就实现了 `IntoResponse`，业务可以直接
//! 构造它并返回，若闸门只写在 `AppError` 上，这条路径就能把 5xx 明细原样送出去。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::errors::code::ErrorCode;

/// `details` 序列化后的体积上限（字节）：超出则整体丢弃，防止响应体被放大。
pub(crate) const MAX_DETAILS_BYTES: usize = 4 * 1024;

/// `details` 允许的最大嵌套深度。
pub(crate) const MAX_DETAILS_DEPTH: usize = 3;

/// 统一响应体。
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ApiResponse<T> {
    /// 业务错误码，`0` 表示成功。
    pub code: i32,
    /// 人类可读信息。
    pub message: String,
    /// 业务数据；失败时为 `null`。
    pub data: Option<T>,
    /// 字段级错误明细；无明细时**整个字段省略**（仅 4xx 可能出现）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// 链路 ID（32 位十六进制 W3C trace-id）。
    pub trace_id: String,
}

impl<T: Serialize> ApiResponse<T> {
    /// 构造成功响应（`code = 0`，`message = "ok"`）。
    #[must_use]
    pub fn ok(data: T) -> Self {
        Self::ok_with_trace(data, crate::telemetry::current_trace_id())
    }

    /// 构造携带指定 `trace_id` 的成功响应。
    #[must_use]
    pub fn ok_with_trace(data: T, trace_id: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::OK.as_i32(),
            message: "ok".to_string(),
            data: Some(data),
            details: None,
            trace_id: trace_id.into(),
        }
    }

    /// 构造携带指定 `code` 与 `data` 的响应。
    ///
    /// 用于需要返回诊断数据但非成功码的场景（如 `/ready` 未就绪时返回 `50005`
    /// 并附带各探针结果）。
    #[must_use]
    pub fn with_data(code: ErrorCode, message: impl Into<String>, data: T) -> Self {
        Self {
            code: code.as_i32(),
            message: message.into(),
            data: Some(data),
            details: None,
            trace_id: crate::telemetry::current_trace_id(),
        }
    }
}

impl ApiResponse<()> {
    /// 构造失败响应（`data = null`，无字段级明细）。
    #[must_use]
    pub fn err(code: ErrorCode, message: impl Into<String>, trace_id: impl Into<String>) -> Self {
        Self::err_with_details(code, message, None, trace_id)
    }

    /// 构造带字段级明细的失败响应（`data = null`）。
    ///
    /// 明细的最终闸门在 [`ApiResponse::into_response`]：**只有 4xx 才会输出**，
    /// 且受深度 / 体积上限约束。这里不做判断，是因为状态码由 `code` 唯一推导，
    /// 构造期还不知道最终会不会落到 5xx（`ErrorCode::new` 会把非法码归一）。
    #[must_use]
    pub fn err_with_details(
        code: ErrorCode,
        message: impl Into<String>,
        details: Option<serde_json::Value>,
        trace_id: impl Into<String>,
    ) -> Self {
        Self {
            code: code.as_i32(),
            message: message.into(),
            data: None,
            details,
            trace_id: trace_id.into(),
        }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(mut self) -> Response {
        // HTTP 状态码由 code 唯一推导（一码一态）；成功即 200。
        // 注意 `0` 必须先判：`ErrorCode::new(0)` 会把成功码归一到 INTERNAL（fail-closed），
        // 直接调用会让成功响应变成 500。
        let status = if self.code == ErrorCode::OK.as_i32() {
            StatusCode::OK
        } else {
            ErrorCode::new(self.code).status()
        };
        // **唯一收口点**：明细只对 4xx 输出（5xx 可能含表名 / 栈帧 / 连接串）。
        self.details = gate_details(self.details, status);
        (status, Json(self)).into_response()
    }
}

/// 字段级明细的闸门：仅 4xx 输出，并受深度 / 体积上限约束。
///
/// 超出上限时**整体丢弃**并告警：宁可少给前端明细，也不让错误响应成为
/// 放大攻击或内部结构泄露的通道。
fn gate_details(
    details: Option<serde_json::Value>,
    status: StatusCode,
) -> Option<serde_json::Value> {
    if !status.is_client_error() {
        return None;
    }
    let details = details?;

    if !within_depth(&details, MAX_DETAILS_DEPTH) {
        tracing::warn!(
            max_depth = MAX_DETAILS_DEPTH,
            "error details 嵌套过深，已丢弃"
        );
        return None;
    }

    match serde_json::to_vec(&details) {
        Ok(bytes) if bytes.len() <= MAX_DETAILS_BYTES => Some(details),
        Ok(bytes) => {
            tracing::warn!(
                len = bytes.len(),
                max = MAX_DETAILS_BYTES,
                "error details 体积超限，已丢弃"
            );
            None
        }
        Err(error) => {
            tracing::warn!(%error, "error details 序列化失败，已丢弃");
            None
        }
    }
}

/// 结构是否在给定深度内（短路递归，避免被超深 JSON 打爆栈）。
fn within_depth(value: &serde_json::Value, remaining: usize) -> bool {
    if remaining == 0 {
        return false;
    }
    match value {
        serde_json::Value::Array(items) => {
            items.iter().all(|item| within_depth(item, remaining - 1))
        }
        serde_json::Value::Object(map) => {
            map.values().all(|item| within_depth(item, remaining - 1))
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn should_serialize_success_envelope() {
        // trace_id 取自请求作用域；作用域外是 `unknown`（见 telemetry::layer）
        let json = crate::telemetry::scope(crate::telemetry::TraceId::new(), async {
            let resp = ApiResponse::ok(serde_json::json!({ "id": 1 })).into_response();
            assert_eq!(resp.status(), http::StatusCode::OK);
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        })
        .await;
        assert_eq!(json["code"], 0);
        assert_eq!(json["message"], "ok");
        assert_eq!(json["data"]["id"], 1);
        assert_eq!(json["trace_id"].as_str().map(str::len), Some(32));
    }

    #[tokio::test]
    async fn should_serialize_error_envelope_with_null_data() {
        let resp =
            ApiResponse::<()>::err(ErrorCode::CONFLICT, "dup", "x".repeat(32)).into_response();
        assert_eq!(resp.status(), http::StatusCode::CONFLICT);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["code"], 30002);
        assert!(json["data"].is_null());
    }

    #[tokio::test]
    async fn should_omit_details_field_when_absent() {
        let resp =
            ApiResponse::<()>::err(ErrorCode::INVALID_PARAM, "bad", "x".repeat(32)).into_response();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json.get("details").is_none(),
            "无明细时 details 字段应整体省略，实际：{json}"
        );
    }

    #[tokio::test]
    async fn should_keep_details_field_when_present() {
        let resp = ApiResponse::<()>::err_with_details(
            ErrorCode::INVALID_PARAM,
            "bad",
            Some(serde_json::json!({ "email": ["格式错误"] })),
            "x".repeat(32),
        )
        .into_response();
        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["details"]["email"][0], "格式错误");
    }

    #[tokio::test]
    async fn should_map_ok_code_to_200() {
        // `ErrorCode::new(0)` 会 fail-closed 归一到 50001，成功响应必须先判 0
        let resp = ApiResponse::ok("done").into_response();
        assert_eq!(resp.status(), http::StatusCode::OK);
    }

    /// 回归：业务直接构造 `ApiResponse` 时，5xx 的 `details` 同样必须被丢弃。
    ///
    /// 修复前闸门只写在 `AppError` 的响应路径里，而 `ApiResponse` 自己实现了
    /// `IntoResponse`——业务只要绕过 `AppError` 就能把内部结构原样送出去。
    #[tokio::test]
    async fn should_drop_details_for_server_error_built_directly() {
        let resp = ApiResponse::<()>::err_with_details(
            ErrorCode::DB_ERROR,
            "数据访问失败",
            Some(serde_json::json!({ "sql": "SELECT * FROM users" })),
            "x".repeat(32),
        )
        .into_response();
        assert_eq!(resp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("SELECT * FROM users"));
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("details").is_none(), "5xx 不得返回 details");
    }

    #[tokio::test]
    async fn should_drop_details_when_too_deep_or_too_large() {
        let huge = serde_json::json!({ "field": "x".repeat(MAX_DETAILS_BYTES + 1) });
        let deep = serde_json::json!({ "a": { "b": { "c": { "d": 1 } } } });

        for details in [huge, deep] {
            let resp = ApiResponse::<()>::err_with_details(
                ErrorCode::INVALID_PARAM,
                "bad",
                Some(details),
                "x".repeat(32),
            )
            .into_response();
            assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(json.get("details").is_none(), "超限明细必须丢弃");
        }
    }
}
