//! axum 集成层：统一响应、统一错误、增强提取器、garde 校验、请求上下文、装配帮手。

pub mod context; // 声明请求上下文子模块 RequestContext
pub mod error; // 声明统一错误子模块 AppError / AppResult
pub mod extractor; // 声明增强提取器子模块（分页/客户端 IP/当前用户）
pub mod response; // 声明统一响应体子模块 ApiResponse / Page
pub mod router; // 声明路由装配帮手子模块（层装配、CORS）
pub mod validate; // 声明 garde 校验提取器子模块 ValidatedJson

pub use context::RequestContext; // 重导出请求上下文，供 handler 直接引用
pub use error::{AppError, AppResult}; // 重导出统一错误类型与结果别名
pub use extractor::{ClientIp, CurrentUser, PageQuery}; // 重导出三个常用提取器
pub use response::{ApiResult, ApiResponse, Page, CODE_OK}; // 重导出统一响应类型与成功码
pub use validate::ValidatedJson; // 重导出带校验的 JSON 提取器
