//! 错误模块。
//!
//! 提供 aurora 的统一错误类型 [`AppError`]、结果别名 [`AppResult`]、业务错误码
//! [`ErrorCode`]，以及到 HTTP 响应的统一映射（`impl IntoResponse`）。
//!
//! 契约见 `docs/09-error-and-response-spec.md`。

pub(crate) mod app_error;
pub(crate) mod code;
mod response;

pub use app_error::{AppError, AppResult};
pub use code::{ErrorCode, InvalidErrorCode};
