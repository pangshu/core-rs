//! `validate()` 入口：把 `validator` 的错误映射为统一错误体。

use crate::errors::AppResult;
use crate::validation::options::{self, ValidationOptions};
use crate::validation::rules;

/// 执行 `value.validate()`（按当前进程的 `[validation]` 配置裁剪明细）。
///
/// # Errors
/// 校验失败时返回 `10001`（400），`details` 为 `{字段路径: [错误信息]}`，
/// 条数受 `[validation].max_errors` / `fail_fast` 约束。
///
/// ```ignore
/// validate(&req)?;   // 失败即 400
/// ```
pub fn validate<T: validator::Validate>(value: &T) -> AppResult<()> {
    validate_with(value, options::global())
}

/// 执行 `value.validate()`，使用**显式**选项而非进程级配置。
///
/// 适合单测、脚本、以及「这条路由要用比全局更严的上限」的场景：
/// 进程级单元格是全局的，靠它做用例隔离会互相干扰。
///
/// # Errors
/// 同 [`validate`]。
pub fn validate_with<T: validator::Validate>(
    value: &T,
    options: ValidationOptions,
) -> AppResult<()> {
    value
        .validate()
        .map_err(|errors| rules::to_app_error(&errors, options))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use validator::Validate;

    use super::*;
    use crate::errors::ErrorCode;

    #[derive(Debug, Deserialize, Validate)]
    struct SignupReq {
        #[validate(email)]
        email: String,
        #[validate(length(min = 8))]
        password: String,
    }

    #[test]
    fn should_pass_when_valid() {
        let req = SignupReq {
            email: "a@b.com".to_string(),
            password: "longenough".to_string(),
        };
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn should_map_validation_failure_to_10001_with_details() {
        let req = SignupReq {
            email: "not-an-email".to_string(),
            password: "short".to_string(),
        };
        let err = validate(&req).unwrap_err();
        assert_eq!(err.code(), ErrorCode::INVALID_PARAM);
        assert_eq!(err.code().status(), axum::http::StatusCode::BAD_REQUEST);

        let details = err.details().expect("details");
        assert!(details["email"].is_array());
        assert!(details["password"].is_array());
    }

    #[test]
    fn should_honour_explicit_options() {
        let req = SignupReq {
            email: "not-an-email".to_string(),
            password: "short".to_string(),
        };
        let err = validate_with(&req, ValidationOptions::with_max_errors(1)).unwrap_err();
        let details = err.details().expect("details").as_object().expect("对象");
        assert_eq!(details.len(), 1, "显式上限应生效，且不依赖全局单元格");
    }
}
