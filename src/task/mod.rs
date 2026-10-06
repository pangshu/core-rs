//! 任务调度（文档 三·19）：定时任务（cron）。与 `queue/` 分工——queue 管
//! **异步消息投递**，task 管**周期调度**；二者可组合（定时任务把活投给队列执行）。
//!
//! - 注册：`App::task(Job::new("cleanup", "0 0 3 * * *", handler))`；
//! - 多实例防重复：`[task].distributed_lock = true` 时经 `cache/lock` 选主执行，
//!   保证同一时刻仅一个实例运行（对标 Celery beat / @Scheduled 的集群语义）；
//! - feature = "scheduler"（tokio-cron-scheduler，6/7 段秒开头 cron 表达式）。

#[cfg(feature = "scheduler")]
pub mod cron;
#[cfg(feature = "scheduler")]
pub mod distributed;
#[cfg(feature = "scheduler")]
pub mod job;

#[cfg(feature = "scheduler")]
pub use job::{Job, JobFuture};


#[cfg(feature = "scheduler")]
use crate::error::{AppError, AppResult};
#[cfg(feature = "scheduler")]
use crate::traits::{HasCache, HasConfig};

/// 注册任务清单并启动调度器（App::serve 内部调用）。
/// `[task].jobs` 配置声明启用的任务（按 name 匹配 handler）；
/// 空 jobs 列表 = 全部未启用，不启动调度器。
#[cfg(feature = "scheduler")]
pub(crate) async fn start_from_config<S>(
    jobs: Vec<Job>,
    state: &S,
) -> AppResult<Option<tokio_cron_scheduler::JobScheduler>>
where
    S: HasCache + HasConfig + Send + Sync + 'static,
{
    let settings = state.config().load().task.clone();
    if !settings.enabled || jobs.is_empty() {
        return Ok(None);
    }

    let scheduler = tokio_cron_scheduler::JobScheduler::new()
        .await
        .map_err(|e| AppError::internal(format!("scheduler init failed: {e}")))?;

    let mut enabled_count = 0;
    for job in &jobs {
        // 配置声明启用的任务才注册；配置里有声明但代码未注册的记 warning
        let Some(decl) = settings.jobs.iter().find(|d| d.name == job.name) else {
            tracing::debug!(name = %job.name, "cron job not enabled in [task].jobs config");
            continue;
        };
        let schedule = decl.schedule.clone();
        let distributed = settings.distributed_lock;
        let tz = decl.timezone.clone();

        let j = cron::make_job(job, &schedule, &tz, distributed, state)?;
        scheduler
            .add(j)
            .await
            .map_err(|e| AppError::internal(format!("cron job {:?} add failed: {e}", job.name)))?;
        tracing::info!(
            name = %job.name,
            schedule = %schedule,
            timezone = %tz,
            distributed,
            "cron job registered"
        );
        enabled_count += 1;
    }

    // 配置声明了但代码没注册的，启动期提示（拼写错误检查）
    for decl in &settings.jobs {
        if !jobs.iter().any(|j| j.name == decl.name) {
            tracing::warn!(
                name = %decl.name,
                "cron job declared in [task].jobs but no handler registered"
            );
        }
    }

    if enabled_count == 0 {
        return Ok(None);
    }

    scheduler
        .start()
        .await
        .map_err(|e| AppError::internal(format!("scheduler start failed: {e}")))?;
    Ok(Some(scheduler))
}
