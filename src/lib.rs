//! # core-rs
//!
//! Rust Web 应用框架（文档 02《Web 应用框架目录结构设计》的实现）：
//! 集成 axum / SeaORM 2 / 配置热更新 / 中间件 / 认证授权 / 可插拔缓存与队列，
//! 应用只写业务。
//!
//! - 入口 [`app::App`]；常用类型统一从 [`prelude`] 导入；
//! - 解耦点 [`traits`]：应用 AppState 内嵌 [`state::CoreState`] 并实现
//!   `HasDb / HasCache / HasQueue / HasConfig` 等 trait，框架零侵入；
//! - 可选能力全部 feature 门控（默认仅 `cache-memory` + `queue-memory` +
//!   `sqlite` + `watch`）：cache-redis / queue-redis /
//!   queue-rabbitmq / queue-kafka / queue-nats / config-remote / session /
//!   jwt / oauth2 / casbin / ws / sse / scheduler / i18n / otel /
//!   rate-limit / csrf / metrics / testing。
//!
//! 判断标准一句话：**把项目名换掉、这段代码仍一字不改 → 进框架；
//! 代码里出现业务词 → 留在应用。**

pub mod app; // App 构建器模块（bootstrap/mount/serve）
pub mod auth; // 认证模块（session/jwt/oauth2 认证链）
#[cfg(feature = "casbin")] // 仅在开启 casbin feature 时编译授权模块
pub mod authz; // 授权模块（Casbin 强制器）
pub mod cache; // 可插拔缓存模块（memory/redis）
pub mod config; // 配置模块（多环境加载与热更新）
pub mod db; // 数据库模块（连接池、CRUD、分页、迁移）
#[cfg(feature = "i18n")] // 仅在开启 i18n feature 时编译国际化模块
pub mod i18n; // 国际化模块
pub mod error; // 统一错误模块（路径别名）
pub mod middleware; // 中间件模块（auth/rate-limit/csrf/ip_filter 等）
pub mod observability; // 可观测性模块（日志、指标、健康检查）
pub mod prelude; // 一站式导入模块
pub mod queue; // 可插拔队列模块（memory/redis/rabbitmq/kafka/nats）
pub mod realtime; // 实时通信模块（ws/sse hub 与跨实例转发）
pub mod resilience; // 弹性模块（超时/重试/熔断等）
pub mod security; // 安全模块（密码哈希等）
pub mod state; // 核心状态模块（CoreState）
pub mod task; // 定时任务模块（scheduler）
pub mod traits; // 解耦点 trait 模块（HasDb/HasCache/…）
pub mod utils; // 工具模块（雪花 ID、时间等）
pub mod web; // web 层模块（响应封装、提取器、路由装配）

#[cfg(feature = "testing")] // 仅在开启 testing feature 时编译
pub mod testing; // 测试辅助模块（TestApp 等）

pub use app::{App, FromCore}; // 在 crate 根重导出入口类型，便于 `core_rs::App`

// 转发底层能力：下游项目原则上只需依赖 core-rs
pub use axum; // 转发 axum
pub use sea_orm; // 转发 sea_orm
pub use serde; // 转发 serde
pub use serde_json; // 转发 serde_json
pub use tokio; // 转发 tokio 运行时
pub use tower; // 转发 tower 中间件库
pub use tracing; // 转发 tracing 日志库

/// 版本号（`env!("CARGO_PKG_VERSION")` 的稳定别名）
pub const VERSION: &str = env!("CARGO_PKG_VERSION"); // 编译期嵌入的 crate 版本号常量
