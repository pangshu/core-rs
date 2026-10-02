//! # core-rs
//!
//! Rust 后端快速开发基础框架（starter 型）：一个依赖、一个 yml、十几行 main.rs 起服务。
//!
//! 薄封装 [axum] + [sea_orm] + Redis，只封装"初始化与胶水"，
//! 底层类型原样透出，随时可绕过封装直接使用原生 API。
//!
//! 入口：[`Application`]；常用类型统一从 [`prelude`] 导入。
//! 可选能力全部 feature 门控：jwt / swagger / metrics / otel / http-client /
//! scheduler / rate-limit / dist-lock / websocket / upload / cli。

pub mod app;
pub mod cache;
pub mod config;
pub mod error;
pub mod logging;
pub mod orm;
pub mod prelude;
pub mod state;
pub mod web;

#[cfg(any(feature = "rate-limit", feature = "dist-lock", feature = "upload"))]
pub mod extra;
#[cfg(feature = "http-client")]
pub mod httpc;
#[cfg(any(feature = "metrics", feature = "otel"))]
pub mod observe;
#[cfg(feature = "queue")]
pub mod queue;
#[cfg(feature = "scheduler")]
pub mod scheduler;
#[cfg(feature = "jwt")]
pub mod security;
#[cfg(feature = "test-util")]
pub mod test;

pub use app::{Application, ApplicationBuilder};

// 转发底层能力：下游项目原则上只需依赖 core-rs
pub use arc_swap;
pub use axum;
pub use deadpool_redis;
pub use sea_orm;
#[cfg(feature = "migration")]
pub use sea_orm_migration;
#[cfg(feature = "swagger")]
pub use utoipa;
pub use serde;
pub use serde_json;
pub use tracing;
pub use validator;
