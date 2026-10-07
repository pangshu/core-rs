//! 防护构件（文档 三·16，配合 middleware）：XSS 清洗、SQL 防注入约定、加密工具。
//! CSRF 由中间件承担（middleware/csrf.rs），本模块只提供底层原语。

pub mod crypto; // 声明加密工具子模块（AES-GCM / RSA / HMAC / 摘要 / 随机数）
pub mod sql_injection; // 声明 SQL 防注入约定子模块（安全 raw 封装 + 标识符白名单）
pub mod xss; // 声明 XSS 防护子模块（输入清洗 / HTML 转义 / 标签剥离）
