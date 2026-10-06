//! cron 表达式调度（含时区），基于 tokio-cron-scheduler（6/7 段，秒开头）。
//! 时区用 IANA 名称（如 `Asia/Shanghai`）；解析失败回落 UTC 并告警。

use std::str::FromStr;

use tokio_cron_scheduler::Job as TokioJob;

use crate::error::{AppError, AppResult};

/// 为任务构造 tokio-cron-scheduler Job；`distributed` 时经 cache/lock 选主。
#[allow(unused_variables)]
pub(crate) fn make_job<S>(
    job: &crate::task::Job,
    schedule: &str,
    timezone: &str,
    distributed: bool,
    state: &S,
) -> AppResult<TokioJob>
where
    S: crate::traits::HasCache + crate::traits::HasConfig + Send + Sync + 'static,
{
    let f = job.handler();

    // 多实例互斥：抢到锁的实例运行，其余实例本轮跳过。
    // 锁获取失败（redis 抖动）按跳过处理——宁可少跑一轮，不可多实例重复跑。
    if distributed {
        let lock = state.lock().clone();
        let lock_ttl =
            std::time::Duration::from_secs(state.config().load().task.lock_ttl_secs.max(60));
        let name = job.name.clone();
        let wrapped = move || {
            let f = f.clone();
            let lock = lock.clone();
            let name = name.clone();
            let lock_ttl = lock_ttl;
            Box::pin(async move {
                match lock.try_acquire(&format!("cron:lock:{name}"), lock_ttl).await {
                    Ok(Some(guard)) => {
                        tracing::debug!(name, "cron job acquired distributed lock");
                        f().await;
                        let _ = guard.release().await;
                    }
                    Ok(None) => {
                        tracing::debug!(name, "cron job skipped: another instance holds the lock")
                    }
                    Err(e) => {
                        tracing::warn!(name, error = %e, "cron job skipped: distributed lock error")
                    }
                }
            }) as crate::task::job::JobFuture
        };
        return with_timezone(schedule, timezone, move |_uuid, _sched| wrapped());
    }

    with_timezone(schedule, timezone, move |_uuid, _sched| f())
}

/// 按 IANA 时区名构造调度 Job（解析失败回落 UTC）
fn with_timezone<F>(schedule: &str, timezone: &str, f: F) -> AppResult<TokioJob>
where
    F: Fn(uuid_cron::Uuid, tokio_cron_scheduler::JobScheduler) -> crate::task::job::JobFuture
        + Send
        + Sync
        + 'static,
{
    if timezone.is_empty() {
        return TokioJob::new_async(schedule, f)
            .map_err(|e| AppError::internal(format!("cron job schedule {schedule:?} invalid: {e}")));
    }
    match chrono_tz::Tz::from_str(timezone) {
        Ok(tz) => {
            #[allow(clippy::let_unit_value)]
            let _ = ();
            TokioJob::new_async_tz(schedule, tz, f).map_err(|e| {
                AppError::internal(format!(
                    "cron job schedule {schedule:?} invalid for tz {timezone}: {e}"
                ))
            })
        }
        Err(_) => {
            tracing::warn!(timezone, "unknown IANA timezone, falling back to UTC");
            TokioJob::new_async(schedule, f)
                .map_err(|e| AppError::internal(format!("cron job schedule {schedule:?} invalid: {e}")))
        }
    }
}

/// tokio-cron-scheduler 的 uuid 再导出别名（Job 回调签名用，避免直接依赖其内部路径）
mod uuid_cron {
    pub use uuid::Uuid;
}
