//! 缓存（feature `cache`）。
//!
//! - [`Cache`]：object-safe 的后端抽象（`get` / `set` / `del` / `take` / `set_nx` /
//!   `del_if_eq` / `exists` / `ping`）
//! - [`CacheService`]：装配入口（`AppState.cache`），提供命名空间、默认 TTL、
//!   **singleflight**（防击穿）、**TTL 抖动**（防雪崩）、**空值缓存**（防穿透）
//! - [`SecureStore`]：T2（语义承载）门面——幂等 / 去重 / 黑名单 / 验证码走这里，
//!   TTL 必填、失败不降级、键按 [`Class`] 隔离
//! - [`MemoryCache`]（`cache-inmem`，默认）/ `RedisCache`（`cache-redis`）
//! - [`DistributedLock`]：分布式锁（Redis `SET NX PX` + Lua compare-and-del）
//!
//! **键布局**：`{namespace}:{class}:{rest}`（见 [`key`]）。`class` 段把业务值 `c:`
//! 与会话 `s:`、幂等 `i:`、锁 `l:`、队列去重 `q:`、实时注册表 `r:`、jti `j:`、
//! 验证码 `o:` 隔开——业务键即使写成 `session:x` 也只会落到 `{ns}:c:session:x`。
//!
//! **一致性范式**：cache-aside + 写后删除；缓存操作不得置于 DB 事务内；
//! 后端异常时**降级直读 DB**（[`CacheService::get_or_set`] 已内置该降级）。
//!
//! **不做**分布式一致性协议（见 `docs/03` §4.11）。
//!
//! 文件划分：`backend`（trait）/ `service`（装配与读写语义）/ `store`（T2 门面）/
//! `key`（键构造）/ `health`（探针）/ `memory`·`redis`（后端实现）/ `lock`（分布式锁）。

mod backend;
mod health;
pub mod key;
pub mod lock;
pub mod memory;
#[cfg(feature = "cache-redis")]
pub mod redis;
mod service;
mod store;

pub use backend::Cache;
pub use health::CacheHealth;
pub use key::Class;
pub use lock::DistributedLock;
pub use memory::MemoryCache;
#[cfg(feature = "cache-redis")]
pub use redis::RedisCache;
pub use service::CacheService;
pub use store::SecureStore;
