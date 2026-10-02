//! Web 层封装：统一响应体、统一错误映射、校验提取器、内置中间件、健康检查。

pub mod extract;
pub mod health;
pub mod middleware;
pub mod response;
