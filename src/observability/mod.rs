//! 可观测性（文档 三·8）：分级日志 + 结构化输出 + 链路追踪 + health / metrics。
//! 运维件：metrics / otel 用 feature 关闭；logging / health 常开。

pub mod health;
pub mod logging;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod tracing;

pub use health::{HealthCheck, HealthStatus};
pub use logging::LogGuard;
