//! 缓存与锁（可插拔：内存 / Redis，配置选择，文档 三·11）。
//!
//! - [`Cache`] / [`Lock`] trait 是唯一契约，业务只依赖 trait，**换后端不改一行业务代码**；
//! - [`memory`]：moka 进程内缓存 + 进程内锁（feature = "cache-memory"，默认），
//!   单机部署与开发/测试的默认值；
//! - [`redis`]：deadpool-redis 连接池 + `SET NX PX` 分布式锁（feature = "cache-redis"），
//!   多实例部署使用；限流 / 幂等 / 分布式锁等需要**跨进程一致**的场景必须 redis；
//! - [`lock`]：`Lock` trait + [`LockGuard`]（释放 Lua 校验持有者 / Drop 兜底），
//!   是限流、防重、幂等的公共底座。

#[cfg(feature = "cache-memory")] // 仅在开启内存缓存 feature 时编译下面模块
pub mod memory; // 进程内 moka 缓存后端（默认）
#[cfg(feature = "cache-redis")] // 仅在开启 Redis 缓存 feature 时编译下面模块
pub mod redis; // Redis 缓存后端与分布式锁实现
pub mod lock; // 锁 trait 与进程内锁实现（两种后端都依赖）

mod build; // 按 [cache] 配置构建缓存实例
mod contract; // 缓存契约（Cache / CacheExt）与共享句柄
mod error; // 缓存统一错误类型

pub use build::build_cache; // 对外导出缓存构建入口
pub use contract::{Cache, CacheExt, CacheHandle}; // 对外导出缓存契约与句柄
pub use error::CacheError; // 对外导出缓存错误类型
pub use lock::{Lock, LockError, LockGuard, LockHandle, MemoryLock, build_lock}; // 对外重导出锁相关公共类型与构建函数
