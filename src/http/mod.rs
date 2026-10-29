//! HTTP 装配模块。
//!
//! 提供 Router 构建器 [`build_router`]、统一响应体 [`ApiResponse`]、请求上下文
//! [`RequestContext`] 与中间件栈装配。

pub mod context;
pub mod extractor;
pub mod middleware;
pub(crate) mod response;
pub mod router;

pub use context::RequestContext;
pub use extractor::{Form, Json, Path, Query};
pub use response::ApiResponse;
pub use router::{AppRouter, AppRoutes, build_router};
