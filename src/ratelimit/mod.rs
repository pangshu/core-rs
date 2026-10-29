//! 限流（feature `ratelimit`）。
//!
//! - [`RateLimitService`]：装配入口（`AppState.rate_limit`），按 `[ratelimit]` 选存储，
//!   并把 `[[ratelimit.rules]]` 编译成「默认桶 + 按路由覆盖的桶」
//! - [`RateLimitStore`]：存储抽象（`memory` = GCRA / `redis` = 固定窗口）
//! - `layer`：中间件（超限 → `40001` + `Retry-After`）
//! - `key`：限流维度（IP / 已验签主体 / 来源不可知）
//!
//! 文件划分：`service`（装配与选桶）/ `store`·`redis`（存储）/ `layer`（中间件）/
//! `key`（维度）/ `throttle`（异常路径的告警节流）。

pub mod key;
pub mod layer;
#[cfg(feature = "ratelimit-redis")]
pub mod redis;
mod service;
pub mod store;
pub(crate) mod throttle;

#[cfg(feature = "ratelimit-redis")]
pub use redis::RedisRateLimitStore;
pub use service::RateLimitService;
pub use store::{MemoryStore, RateLimitStore};
