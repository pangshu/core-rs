//! 防护构件（文档 三·16，配合 middleware）：XSS 清洗、SQL 防注入约定、加密工具。
//! CSRF 由中间件承担（middleware/csrf.rs），本模块只提供底层原语。

pub mod crypto;
pub mod sql_injection;
pub mod xss;
