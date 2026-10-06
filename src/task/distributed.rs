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

use std::sync::Arc;
use std::time::Duration;

use crate::cache::lock::{Lock, LockError};

/// 尝试以 `name` 选主（TTL 内持有）；返回 None 表示已有主。
/// `[task].distributed_lock = true` 的 cron 任务自动走此机制（见 task/cron.rs）。
pub async fn elect_leader(
    lock: Arc<dyn Lock>,
    name: &str,
    ttl: Duration,
) -> Result<Option<crate::cache::lock::LockGuard>, LockError> {
    lock.try_acquire(&format!("core-rs:leader:{name}"), ttl).await
}
