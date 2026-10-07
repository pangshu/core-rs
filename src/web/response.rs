//! 统一响应体 `{ code, message, data }`。成功 code 固定为 [`CODE_OK`]（0），
//! 失败路径由 [`crate::web::error::AppError`] 生成同构响应。

use axum::http::StatusCode; // 引入 HTTP 状态码类型，用于统一响应构造
use axum::response::{IntoResponse, Response}; // 引入可转响应 trait 与响应类型
use axum::Json; // 引入 axum 的 JSON 响应包装器
use serde::{Deserialize, Serialize}; // 引入序列化/反序列化派生宏

use crate::web::error::AppError; // 引入统一错误类型，用于 ApiResult 别名

/// 业务成功码。错误响应的 code：HTTP 映射类等于状态码（400/401/403/404/429/500），
/// 6401 保留给「token 过期需刷新」，≥1000 为应用自定义业务码段（文档 三·7）。
pub const CODE_OK: i32 = 0; // 定义成功响应的业务码常量 0

/// handler 统一返回类型：`?` 直接抛 [`AppError`]，成功值经 `ApiResponse::ok` 包装。
///
/// axum 对 `Result<T, E>`（两侧均实现 IntoResponse）有 blanket 实现，
/// 因此无需为该别名单独实现 IntoResponse。
pub type ApiResult<T> = Result<ApiResponse<T>, AppError>; // 定义 handler 统一返回类型别名

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆/序列化/反序列化
pub struct ApiResponse<T> { // 定义泛型统一响应体结构体
    pub code: i32, // 业务码，成功为 0
    pub message: String, // 提示消息
    pub data: T, // 业务数据载荷
}

impl<T> ApiResponse<T> { // 为任意数据类型的响应体实现方法
    pub fn ok(data: T) -> Self { // 构造成功响应（code=0，message="ok"）
        Self { // 构造自身实例
            code: CODE_OK, // 业务码置为成功码 0
            message: "ok".to_string(), // 固定成功消息
            data, // 透传业务数据
        }
    }

    pub fn with_msg(message: impl Into<String>, data: T) -> Self { // 构造带自定义消息的成功响应
        Self { // 构造自身实例
            code: CODE_OK, // 业务码仍为成功码 0
            message: message.into(), // 转换并存入自定义消息
            data, // 透传业务数据
        }
    }
}

impl ApiResponse<()> { // 为无数据载荷（单元类型）的响应体实现方法
    pub fn error(code: i32, message: impl Into<String>) -> Self { // 构造错误响应（data 为 null/unit）
        Self { // 构造自身实例
            code, // 透传调用方给出的业务码
            message: message.into(), // 转换并存入错误消息
            data: (), // 错误响应无数据载荷
        }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> { // 让响应体可直接作为 axum 响应返回
    fn into_response(self) -> Response { // 实现转换为 HTTP 响应
        (StatusCode::OK, Json(self)).into_response() // 固定 200 状态码并以 JSON 输出响应体
    }
}

/// 统一分页响应（与 [`crate::db::paginate`] 的 ORM 分页字段对齐）
#[derive(Debug, Clone, Serialize)] // 派生调试/克隆/序列化（分页只出不出入）
pub struct Page<T> { // 定义泛型分页响应结构体
    pub records: Vec<T>, // 当前页记录列表
    pub total: u64, // 记录总数
    pub page: u64, // 当前页码（1 起始）
    pub size: u64, // 每页条数
    pub pages: u64, // 总页数
}

impl<T> Page<T> { // 为分页响应实现方法
    pub fn new(records: Vec<T>, total: u64, page: u64, size: u64) -> Self { // 构造分页响应并自动算总页数
        let pages = if size == 0 { 0 } else { total.div_ceil(size) }; // 总页数向上取整，size 为 0 时避免除零
        Self { // 构造自身实例
            records, // 透传当前页记录
            total, // 透传记录总数
            page, // 透传当前页码
            size, // 透传每页条数
            pages, // 存入计算出的总页数
        }
    }
}
