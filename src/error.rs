//! 统一错误（实现在 [`crate::web::error`]，此处为 crate 根路径别名：
//! `crate::error::AppError` 与 `crate::web::error::AppError` 同一类型）。

pub use crate::web::error::{ValidationItem, AppError, AppResult}; // 从 web::error 重导出错误类型，形成根路径别名
