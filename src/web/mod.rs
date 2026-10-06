//! axum 集成层：统一响应、统一错误、增强提取器、garde 校验、请求上下文、装配帮手。

pub mod context;
pub mod error;
pub mod extractor;
pub mod response;
pub mod router;
pub mod validate;

pub use context::RequestContext;
pub use error::{AppError, AppResult};
pub use extractor::{ClientIp, CurrentUser, PageQuery};
pub use response::{ApiResult, ApiResponse, Page, CODE_OK};
pub use validate::ValidatedJson;
