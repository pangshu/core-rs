//! 中间件库（01 文档的实现整体上移）：一个关注点一个文件。
//!
//! ## 推荐装配顺序（由外到内；`App::serve` 按此自动装配）
//!
//! ```text
//! panic → request_id → locale → trace → access_log
//!       → security_headers → timeout → cors → ip_filter
//!       → rate_limit → csrf → auth → idempotency → handler
//! ```
//!
//! authz 不在链上：授权走路由级 `required()`（声明即校验，见 middleware/authz.rs）；
//! locale 固定在 request_id 之后（由 web/router.rs::base_layers 装配）。
//!
//! - 请求体大小限制用 axum 内置 `DefaultBodyLimit`（web/router.rs 在推荐位次挂载）；
//! - 组合权留应用：挂什么、挂哪层由应用的路由树决定；
//!   应用若偏离推荐顺序，以本模块注释为据。

pub mod access_log;
pub mod auth;
pub mod cors;
pub mod idempotency;
pub mod ip_filter;
pub mod locale;
pub mod panic;
#[cfg(feature = "rate-limit")]
pub mod rate_limit;
pub mod request_id;
pub mod security_headers;
pub mod timeout;

#[cfg(feature = "csrf")]
pub mod csrf;
#[cfg(feature = "casbin")]
pub mod authz;

#[cfg(feature = "casbin")]
pub use authz::{required, required_in, RequiredPermission};
