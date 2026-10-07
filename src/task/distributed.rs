//! 多实例防重复（文档 三·19）：经 `cache/lock` 选主，保证同一时刻仅一个实例
//! 执行。本模块提供独立于 cron 的「手动选主」助手——应用自定义循环任务时使用。
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # async fn demo(lock: core_rs::cache::LockHandle) -> Result<(), LockError> {
//! // 非调度器场景：自定义长循环里自行抢锁
//! if let Some(guard) = lock.clone().try_acquire("my-loop-leader", std::time::Duration::from_secs(600)).await? {
//!     // 仅主实例进入
//!     let _ = guard.release().await;
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc; // 引入 Arc，用于共享锁句柄
use std::time::Duration; // 引入 Duration，表示选主锁的 TTL

use crate::cache::lock::{Lock, LockError}; // 引入锁 trait 与锁错误类型

/// 尝试以 `name` 选主（TTL 内持有）；返回 None 表示已有主。
/// `[task].distributed_lock = true` 的 cron 任务自动走此机制（见 task/cron.rs）。
pub async fn elect_leader( // 手动选主：抢到锁者成为 leader
    lock: Arc<dyn Lock>, // 分布式锁句柄
    name: &str, // 选主名（同一 name 互斥）
    ttl: Duration, // 锁持有时间
) -> Result<Option<crate::cache::lock::LockGuard>, LockError> { // 返回锁守卫；None 表示已有主
    lock.try_acquire(&format!("core-rs:leader:{name}"), ttl).await // 拼装锁键并尝试非阻塞获取
}
