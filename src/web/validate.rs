//! garde 集成（文档 三·9）：`ValidatedJson<T>` 完成「提取 → 校验 → 错误映射」。
//!
//! 校验规则写在 DTO 上（`#[garde(...)]`），DTO 仍在应用侧；校验失败把 garde 的
//! `Report` 统一映射成 [`AppError::Validation`]，错误明细进 `ApiResponse.data`，
//! 与手写校验、`Biz` 业务码共存。框架走无上下文路径（`Context = ()`）。

use axum::extract::{FromRequest, Request};
use axum::Json;
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// `Json<T>` + `garde::Validate` 组合提取器。用法：`ValidatedJson(dto): ValidatedJson<CreateUser>`。
#[derive(Debug, Clone)]
pub struct ValidatedJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned + garde::Validate<Context = ()>,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state)
            .await
            .map_err(|rej| AppError::bad_request(rej.body_text()))?;
        value.validate()?;
        Ok(ValidatedJson(value))
    }
}
