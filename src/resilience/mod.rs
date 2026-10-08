//! 弹性：熔断 / 重试 / 降级 / 舱壁（文档 三·7）。外部依赖（第三方 API、DB、缓存）
//! 抖动时按策略隔离，避免级联故障。纯 tokio 实现，无外部依赖，常开。
//!
//! 策略配置集中在 `[resilience]`（各依赖的阈值 / 重试次数 / 降级开关）：
//!
//! ```toml
//! [resilience.default]
//! failure_threshold = 5
//! open_secs = 30
//! max_retries = 2
//!
//! [resilience.deps."payment-api"]
//! failure_threshold = 3
//! fallback_enabled = true
//! ```

pub mod bulkhead; // 声明舱壁子模块（并发隔离）
pub mod circuit_breaker; // 声明熔断器子模块
pub mod fallback; // 声明降级子模块
pub mod retry; // 声明重试子模块

mod error; // 弹性调用错误类型（熔断拒绝 / 重试耗尽）
mod registry; // 弹性组件注册表（按依赖名持有熔断器与舱壁）

pub use error::ResilienceError; // 对外导出弹性调用错误类型
pub use registry::Registry; // 对外导出弹性组件注册表
pub use retry::Backoff; // 对外导出退避时长别名
