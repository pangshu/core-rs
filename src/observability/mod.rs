//! 可观测性（文档 三·8）：分级日志 + 结构化输出 + 链路追踪 + health / metrics。
//! 运维件：metrics / otel 用 feature 关闭；logging / health 常开。

pub mod health; // 导出健康检查（/health、/ready）子模块
pub mod logging; // 导出日志初始化与 guard 子模块
#[cfg(feature = "metrics")] // 仅在开启 metrics feature 时编译下面的模块
pub mod metrics; // 导出 Prometheus 指标子模块
pub mod tracing; // 导出链路追踪（OTel）子模块

pub use health::{HealthCheck, HealthStatus}; // 对外重导出健康探针契约与状态枚举
pub use logging::LogGuard; // 对外重导出日志保活句柄
