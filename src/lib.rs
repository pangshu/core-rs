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
//!   `sqlite` + `migration` + `watch`）：cache-redis / queue-redis /
//!   queue-rabbitmq / queue-kafka / queue-nats / config-remote / session /
//!   jwt / oauth2 / casbin / ws / sse / scheduler / i18n / otel /
//!   rate-limit / csrf / metrics / testing。
//!
//! 判断标准一句话：**把项目名换掉、这段代码仍一字不改 → 进框架；
//! 代码里出现业务词 → 留在应用。**

pub mod app;
pub mod auth;
#[cfg(feature = "casbin")]
pub mod authz;
pub mod cache;
pub mod config;
pub mod db;
#[cfg(feature = "i18n")]
pub mod i18n;
pub mod error;
pub mod middleware;
pub mod observability;
pub mod prelude;
pub mod queue;
pub mod realtime;
pub mod resilience;
pub mod security;
pub mod state;
pub mod task;
pub mod traits;
pub mod utils;
pub mod web;

#[cfg(feature = "testing")]
pub mod testing;

pub use app::{App, FromCore};

// 转发底层能力：下游项目原则上只需依赖 core-rs
#[cfg(feature = "migration")]
pub use sea_orm_migration;
pub use axum;
pub use sea_orm;
pub use serde;
pub use serde_json;
pub use tokio;
pub use tower;
pub use tracing;

/// 版本号（`env!("CARGO_PKG_VERSION")` 的稳定别名）
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
