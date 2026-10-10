//! 一站式导入：业务项目里 `use core_rs::prelude::*;` 即可拿到全部常用类型，
//! 包括 axum / sea_orm 等底层 crate 的常用项（下游无需直接依赖它们）。

// 框架核心
pub use crate::app::{App, FromCore}; // 导出 App 构建器与状态构造 trait
pub use crate::error::{AppError, AppResult}; // 导出统一错误与结果类型
pub use crate::state::CoreState; // 导出框架核心状态
pub use crate::traits::{HasAuth, HasCache, HasConfig, HasDb, HasHealthChecks, HasQueue}; // 导出各能力 trait
#[cfg(any(feature = "ws", feature = "sse"))] // 开启 ws 或 sse 时才导出实时通信 trait
pub use crate::traits::HasRealtime; // 导出实时通信能力 trait

// 配置
pub use crate::config::{ // 导出配置模块相关类型
    self, ConfigHandle, Environment, LoadOptions, OnChange, Settings, // 模块本身、句柄、环境、加载选项、变更回调、设置
};

// 中间件（裸模式自组装：各模块 layer() 构造器 + stack 预设栈；
// BoxedLayer 为构造器统一返回类型）
pub use crate::middleware::{ // 导出中间件模块与层类型
    access_log, auth, idempotency, ip_filter, locale, panic, request_id, // 无状态件与认证
    security_headers, stack, timeout, // 安全头、预设栈与超时
    BoxedLayer, // 自组装层统一返回类型
};
#[cfg(feature = "csrf")] // 仅在开启 csrf feature 时导出
pub use crate::middleware::csrf; // CSRF 防护模块
#[cfg(feature = "rate-limit")] // 仅在开启 rate-limit feature 时导出
pub use crate::middleware::rate_limit; // 固定窗口限流模块
#[cfg(feature = "casbin")] // 仅在开启 casbin feature 时导出
pub use crate::middleware::authz; // 授权模块（路由级 required()）

// web 层
pub use crate::error::ValidationItem; // 导出字段级校验错误项
pub use crate::web::{ // 导出 web 层常用类型
    ApiResult, ApiResponse, ClientIp, CurrentUser, Page, PageQuery, RequestContext, // 统一响应、客户端 IP、当前用户、分页与请求上下文
    ValidatedJson, CODE_OK, // 校验 JSON 提取器与成功状态码常量
};

// db
pub use crate::db::{ // 导出数据库相关类型
    cursor::{CursorPage, CursorQuery}, // 游标分页结果与查询参数
    paginate::{PageParams, Paginated}, // 页码分页参数与结果
    Crud, CrudExt, PkOf, // CRUD trait、扩展方法与主键关联类型
};

// cache
pub use crate::cache::{Cache, CacheError, CacheExt, CacheHandle}; // 导出缓存 trait、错误、扩展方法与句柄

// queue
pub use crate::queue::{Message as QueueMessage, Queue, QueueError, QueueHandle}; // 导出队列消息（重命名）、trait、错误与句柄

// observability
pub use crate::observability::{HealthCheck, HealthStatus}; // 导出健康探针 trait 与状态枚举

// tls（服务端 TLS 契约）
#[cfg(feature = "tls")] // 仅在开启 tls feature 时导出
pub use crate::tls::{CertEntry, CertProvider, TlsState}; // 导出证书来源契约、DTO 与 TLS 状态

// utils
pub use crate::utils::snowflake::Snowflake; // 导出雪花 ID 生成器
pub use crate::utils::time; // 导出时间工具模块

// 底层 crate 常用项（下游无需直接依赖）
pub use axum::{ // 转发 axum 常用项
    extract::{ConnectInfo, Path, Query, State}, // 常用提取器
    routing::{delete, get, post, put}, // 常用路由方法
    Json, Router, // JSON 提取/响应与路由树
};
#[cfg(feature = "ws")] // 仅在开启 ws feature 时导出
pub use axum::extract::ws::WebSocketUpgrade; // 导出 WebSocket 升级提取器
pub use chrono; // 转发 chrono 时间库
pub use sea_orm::{ // 转发 sea_orm 常用项
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, ConnectOptions, Database, // 模型/字段/连接相关 trait 与类型
    DatabaseConnection, DbErr, DeriveEntityModel, EntityTrait, PaginatorTrait, QueryFilter, // 连接、错误、派生宏、实体与分页/过滤
    QueryOrder, Set, TransactionTrait, // 排序、赋值与事务
};
pub use serde::{Deserialize, Serialize}; // 转发 serde 序列化/反序列化派生宏
pub use serde_json; // 转发 serde_json
pub use tracing; // 转发 tracing 日志库
