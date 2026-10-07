//! 统一错误 [`AppError`]：业务代码只管返回这个类型，框架负责映射为 HTTP 状态码
//! 和统一响应体 `{ code, message, data }`。内部错误细节只进日志，不外泄给客户端。
//!
//! 业务码分段约定（文档 三·7）：
//! - `0` 成功；`4xx/5xx` 与 HTTP 状态码对齐；`6401` 保留（token 过期需刷新）；
//! - `≥1000` 为应用自定义业务码段（[`AppError::Biz`]），HTTP 统一 422。

use axum::http::StatusCode; // 引入 HTTP 状态码类型，用于错误映射
use axum::response::{IntoResponse, Response}; // 引入可转响应 trait 与响应类型
use axum::Json; // 引入 axum 的 JSON 响应包装器

use crate::cache::CacheError; // 引入缓存错误类型，用于 #[from] 自动转换
use crate::web::response::ApiResponse; // 引入统一响应体，用于错误响应构造

/// 应用层返回值别名
pub type AppResult<T> = Result<T, AppError>; // 定义应用层统一结果别名

#[derive(Debug, thiserror::Error)] // 派生调试输出并生成 std::error::Error 实现
pub enum AppError { // 定义框架统一错误枚举
    #[error("{0}")] // 错误 Display 直接使用内部消息
    BadRequest(String), // 400 请求参数错误

    #[error("{0}")] // 错误 Display 直接使用内部消息
    Unauthorized(String), // 401 未认证

    /// token 签名合法但已过期：HTTP 仍为 401，响应体 `code = 6401`（保留码，
    /// 前端据此区分「需要刷新」与「未认证」）
    #[error("{0}")] // 错误 Display 直接使用内部消息
    TokenExpired(String), // 401+6401 token 过期（需刷新）

    #[error("{0}")] // 错误 Display 直接使用内部消息
    Forbidden(String), // 403 已认证但无权限

    #[error("{0}")] // 错误 Display 直接使用内部消息
    NotFound(String), // 404 资源不存在

    #[error("{0}")] // 错误 Display 直接使用内部消息
    TooManyRequests(String), // 429 触发限流

    /// 应用自定义业务码（≥1000），HTTP 422；消息按 locale 翻译（feature = "i18n"）
    #[error("{1}")] // Display 取第二个字段（消息文本），忽略业务码
    Biz(i32, String), // 应用自定义业务码 + 消息

    /// 参数校验失败（garde 报告，归一化为 字段路径 → 消息；进响应 data 字段）
    #[error("validation failed")] // Display 固定文案，明细走 data 字段
    Validation(Vec<ValidationItem>), // 校验错误明细列表

    #[error("database error: {0}")] // Display 带前缀，包裹底层 DB 错误
    Db(#[from] sea_orm::DbErr), // 由 SeaORM 数据库错误自动转换而来

    #[error("cache error: {0}")] // Display 带前缀，包裹底层缓存错误
    Cache(#[from] CacheError), // 由缓存错误自动转换而来

    #[error("queue error: {0}")] // Display 带前缀，包裹底层队列错误
    Queue(#[from] crate::queue::QueueError), // 由队列错误自动转换而来

    #[error("config error: {0}")] // Display 带前缀，包裹底层配置错误
    Config(#[from] config::ConfigError), // 由配置库错误自动转换而来

    #[error("io error: {0}")] // Display 带前缀，包裹底层 IO 错误
    Io(#[from] std::io::Error), // 由标准库 IO 错误自动转换而来

    #[error("{0}")] // 错误 Display 直接使用内部消息
    Internal(String), // 500 服务端内部错误（细节不外泄）
}

impl AppError { // 为 AppError 提供构造与映射方法
    pub fn bad_request(msg: impl Into<String>) -> Self { // 构造 400 错误
        Self::BadRequest(msg.into()) // 转换消息并包成 BadRequest
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self { // 构造 401 未认证错误
        Self::Unauthorized(msg.into()) // 转换消息并包成 Unauthorized
    }

    pub fn forbidden(msg: impl Into<String>) -> Self { // 构造 403 无权限错误
        Self::Forbidden(msg.into()) // 转换消息并包成 Forbidden
    }

    pub fn not_found(msg: impl Into<String>) -> Self { // 构造 404 未找到错误
        Self::NotFound(msg.into()) // 转换消息并包成 NotFound
    }

    pub fn too_many_requests(msg: impl Into<String>) -> Self { // 构造 429 限流错误
        Self::TooManyRequests(msg.into()) // 转换消息并包成 TooManyRequests
    }

    pub fn biz(code: i32, msg: impl Into<String>) -> Self { // 构造应用自定义业务码错误
        debug_assert!(code >= 1000, "业务码约定 ≥1000，避免与 HTTP 映射码冲突"); // 调试期断言业务码 ≥1000
        Self::Biz(code, msg.into()) // 包成 Biz 变体返回
    }

    pub fn internal(msg: impl Into<String>) -> Self { // 构造 500 内部错误
        Self::Internal(msg.into()) // 转换消息并包成 Internal
    }

    /// HTTP 状态码与响应体 code（公开给业务/测试做错误码断言）
    pub fn status_and_code(&self) -> (StatusCode, i32) { // 计算该错误对应的 HTTP 状态码与业务码
        use AppError::*; // 局部引入各变体，简化 match 写法
        match self { // 按错误变体分派映射
            BadRequest(_) | Validation(_) => (StatusCode::BAD_REQUEST, 400), // 参数错误/校验失败 → 400
            Unauthorized(_) => (StatusCode::UNAUTHORIZED, 401), // 未认证 → 401
            // 过期与未认证 HTTP 同为 401，靠响应体 code 6401 区分
            TokenExpired(_) => (StatusCode::UNAUTHORIZED, 6401), // token 过期 → 401 + 保留码 6401
            Forbidden(_) => (StatusCode::FORBIDDEN, 403), // 无权限 → 403
            NotFound(_) => (StatusCode::NOT_FOUND, 404), // 资源不存在 → 404
            TooManyRequests(_) => (StatusCode::TOO_MANY_REQUESTS, 429), // 限流 → 429
            // 业务码段：HTTP 422，body.code 透传应用自定义码
            Biz(code, _) => (StatusCode::UNPROCESSABLE_ENTITY, *code), // 业务错误 → 422 + 透传自定义码
            Db(_) | Cache(_) | Config(_) | Io(_) | Internal(_) => { // 各类基础设施/内部错误
                (StatusCode::INTERNAL_SERVER_ERROR, 500) // 统一映射为 500
            }
            Queue(_) => (StatusCode::INTERNAL_SERVER_ERROR, 500), // 队列错误同样映射为 500
        }
    }

    /// 5xx 细节只记日志，对外统一话术；4xx 语义原样输出
    pub(crate) fn client_message(&self) -> String { // 生成可安全返回给客户端的消息
        let (status, _) = self.status_and_code(); // 先取状态码，用于判断是否为服务端错误
        if status.is_server_error() { // 若为 5xx
            tracing::error!(error = %self, "internal error"); // 把真实错误细节写入日志
            "internal server error".to_string() // 对外仅返回统一话术，避免信息泄露
        } else { // 4xx 等客户端错误
            self.to_string() // 直接输出原错误消息，语义明确
        }
    }
}

impl From<garde::Report> for AppError { // 让 garde 校验报告可直接 `?` 转为 AppError
    fn from(report: garde::Report) -> Self { // 实现转换逻辑
        AppError::Validation(ValidationItem::from_report(&report)) // 归一化为校验错误明细列表
    }
}

/// 校验错误明细（字段路径 → 消息），用于响应体 data 字段
#[derive(Debug, Clone, serde::Serialize)] // 派生调试/克隆/序列化，便于放入响应
pub struct ValidationItem { // 定义单条校验错误结构体
    pub field: String, // 出错字段路径
    pub message: String, // 该字段的校验错误消息
}

impl ValidationItem { // 为校验错误明细实现转换方法
    pub fn from_report(report: &garde::Report) -> Vec<Self> { // 把 garde 报告转成明细列表
        report // 遍历报告
            .iter() // 逐条读取 (路径, 错误) 对
            .map(|(path, err)| Self { // 每条映射为 ValidationItem
                field: path.to_string(), // 字段路径转字符串
                message: err.to_string(), // 校验错误转字符串
            })
            .collect() // 收集成 Vec 返回
    }
}

impl IntoResponse for AppError { // 让 AppError 可直接作为 axum 响应返回
    fn into_response(self) -> Response { // 实现转换为 HTTP 响应
        let (status, code) = self.status_and_code(); // 取得 HTTP 状态码与业务码
        let message = self.client_message(); // 取得可安全外泄的提示消息
        let body: ApiResponse<serde_json::Value> = match &self { // 构造统一响应体，按变体区分 data
            AppError::Validation(items) => ApiResponse { // 校验失败：data 带明细
                code, // 透传业务码
                message: message.clone(), // 复制提示消息
                data: serde_json::to_value(items).unwrap_or(serde_json::Value::Null), // 明细序列化，失败则置 null
            },
            _ => ApiResponse { // 其余错误：data 为空
                code, // 透传业务码
                message, // 移动提示消息
                data: serde_json::Value::Null, // 无附加数据
            },
        };
        (status, Json(body)).into_response() // 组合状态码与 JSON 体输出响应
    }
}
