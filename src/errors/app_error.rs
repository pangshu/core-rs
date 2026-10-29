//! 统一错误类型 [`AppError`] 与结果别名 [`AppResult`]。
//!
//! 契约见 `docs/09-error-and-response-spec.md` §4：
//! 对外只暴露 `code` + 固定文案，内部细节（`anyhow::Error`）仅进日志。

use crate::errors::code::ErrorCode;

/// aurora 的统一错误类型。
///
/// 模块**不自定义对外错误类型**，一律映射到本类型（业务领域错误由应用侧
/// `impl From<领域错误> for AppError` 提供转换）。
///
/// # 泄露边界
///
/// 只有 [`AppError::Biz`] 的 `message` / `details` 会对外返回，**由调用方负责**
/// 不在其中放入内部细节（SQL、DSN、内网地址、堆栈等）。
/// [`AppError::Internal`] 的 `Display` 是**固定文案**，内部细节只能经
/// [`AppError::inner`] 取出供日志使用——即使有人误把错误拼进对外文案，
/// 也不会把内部错误文本带出去。
#[derive(Debug, thiserror::Error)]
#[must_use = "错误必须被处理或上报，静默丢弃会丢失故障与审计信息"]
#[non_exhaustive]
pub enum AppError {
    /// 业务错误：携带稳定错误码、人类可读信息与可选字段级明细。
    #[error("{message}")]
    Biz {
        /// 业务错误码（决定 HTTP 状态码）。
        code: ErrorCode,
        /// 面向开发者 / 前端的人类可读信息。
        message: String,
        /// 可选的字段级错误明细（仅 4xx 返回）。
        details: Option<serde_json::Value>,
    },

    /// 内部错误：包装 `anyhow::Error`，**不对外泄露内容**。
    ///
    /// `Display` 固定为「服务内部错误」（与 `ErrorCode::INTERNAL` 的默认文案一致），
    /// 不使用 `transparent`——否则任何 `to_string()` / `format!("{err}")` 都会把
    /// 内部错误链（含连接串、SQL、内网地址）带进对外响应。
    #[error("服务内部错误")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    /// 由错误码与信息构造业务错误。
    ///
    /// `code` 为 [`ErrorCode::OK`] 时归一到 [`ErrorCode::INTERNAL`]：
    /// 成功码不能表示错误，否则会以 HTTP 200 返回错误体（fail-open）。
    pub fn biz(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Biz {
            code: sanitize_code(code),
            message: message.into(),
            details: None,
        }
    }

    /// 由错误码、信息与字段级明细构造业务错误。
    ///
    /// `details` 仅在对应 HTTP 状态码为 4xx 时才会返回给客户端（见
    /// `errors::response`），5xx 一律丢弃。
    pub fn biz_with_details(
        code: ErrorCode,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self::Biz {
            code: sanitize_code(code),
            message: message.into(),
            details: Some(details),
        }
    }

    /// 参数校验失败（`10001` → 400）。
    pub fn invalid_param(message: impl Into<String>) -> Self {
        Self::biz(ErrorCode::INVALID_PARAM, message)
    }

    /// 未认证（`20001` → 401）。
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::biz(ErrorCode::UNAUTHENTICATED, message)
    }

    /// 无权限（`20002` → 403）。
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::biz(ErrorCode::FORBIDDEN, message)
    }

    /// 资源不存在（`30001` → 404）。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::biz(ErrorCode::NOT_FOUND, message)
    }

    /// 资源冲突（`30002` → 409）。
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::biz(ErrorCode::CONFLICT, message)
    }

    /// 请求过于频繁（`40001` → 429）。
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::biz(ErrorCode::RATE_LIMITED, message)
    }

    /// 服务内部错误（`50001` → 500）。
    pub fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }

    /// 返回该错误对外暴露的业务错误码。
    ///
    /// [`AppError::Internal`] 统一映射为 [`ErrorCode::INTERNAL`]。
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Biz { code, .. } => *code,
            Self::Internal(_) => ErrorCode::INTERNAL,
        }
    }

    /// 返回可选的字段级明细。
    #[must_use]
    pub fn details(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Biz { details, .. } => details.as_ref(),
            Self::Internal(_) => None,
        }
    }

    /// 返回内部错误（仅 [`AppError::Internal`] 有值）。
    ///
    /// **仅供日志 / 诊断使用**，返回值不得写入任何对外响应或对外文案。
    #[must_use]
    pub fn inner(&self) -> Option<&anyhow::Error> {
        match self {
            Self::Internal(err) => Some(err),
            Self::Biz { .. } => None,
        }
    }
}

/// 把 [`ErrorCode::OK`] 归一到 [`ErrorCode::INTERNAL`]。
///
/// 成功码不能承载错误：否则错误会以 HTTP 200 返回，客户端按状态码判断时
/// 会把失败当成功（fail-open）。
const fn sanitize_code(code: ErrorCode) -> ErrorCode {
    if code.as_i32() == ErrorCode::OK.as_i32() {
        ErrorCode::INTERNAL
    } else {
        code
    }
}

/// aurora 的统一结果别名。
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_biz_code_when_biz_error() {
        let err = AppError::not_found("user 1 not found");
        assert_eq!(err.code(), ErrorCode::NOT_FOUND);
        assert!(err.details().is_none());
    }

    #[test]
    fn should_map_internal_to_internal_code_when_internal_error() {
        let err = AppError::internal(anyhow::anyhow!("boom"));
        assert_eq!(err.code(), ErrorCode::INTERNAL);
    }

    #[test]
    fn should_keep_details_when_provided() {
        let err = AppError::biz_with_details(
            ErrorCode::INVALID_PARAM,
            "bad",
            serde_json::json!({ "field": "email" }),
        );
        assert_eq!(
            err.details()
                .and_then(|d| d.get("field"))
                .and_then(|v| v.as_str()),
            Some("email")
        );
    }

    #[test]
    fn should_not_leak_internal_detail_through_display() {
        let err = AppError::internal(anyhow::anyhow!("postgres://user:pw@10.0.0.1/app"));
        // Display 固定文案：任何 `to_string()` / `format!("{err}")` 都漏不出去
        assert_eq!(err.to_string(), "服务内部错误");
        assert!(
            !format!("{err:?}").is_empty(),
            "Debug 仍应保留内部细节供日志"
        );
        assert!(err.inner().is_some());
    }

    #[test]
    fn should_not_leak_internal_detail_when_nested_in_biz_message() {
        let inner = AppError::internal(anyhow::anyhow!("secret db dsn"));
        let wrapped = AppError::biz(ErrorCode::UPSTREAM_ERROR, format!("{inner}"));
        assert_eq!(wrapped.to_string(), "服务内部错误");
    }

    #[test]
    fn should_normalize_ok_code_to_internal() {
        // 成功码不能表示错误，否则会以 200 返回（fail-open）
        let err = AppError::biz(ErrorCode::OK, "should be an error");
        assert_eq!(err.code(), ErrorCode::INTERNAL);
        assert_eq!(err.code().status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}
