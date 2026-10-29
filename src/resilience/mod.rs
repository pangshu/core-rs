//! 韧性（feature `resilience`）。
//!
//! 作用于**出站调用**（不参与入站中间件链，见 `docs/02` §11）：
//!
//! - [`CircuitBreaker`]：熔断（`Closed` / `Open` / `HalfOpen`）
//! - [`RetryPolicy`]：重试（仅对 5xx 类错误）
//! - [`Fallback`] / [`with_fallback`]：降级
//! - [`Bulkhead`] / [`with_timeout`]：舱壁隔离与**出站**超时
//! - [`PolicyRegistry`]：按 `[resilience]` 一次性构建并组合上述策略
//!
//! **不做**服务网格级治理（进程内韧性即可，见 `docs/03` §4.19）。

pub mod bulkhead;
pub mod circuit_breaker;
pub mod fallback;
pub mod registry;
pub mod retry;

pub use bulkhead::{Bulkhead, with_timeout};
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitState, ProbeGuard, is_degradable, is_retryable,
    is_self_rejection,
};
pub use fallback::{Fallback, with_fallback};
pub use registry::PolicyRegistry;
pub use retry::RetryPolicy;
