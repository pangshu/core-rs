//! cron 定时任务（feature = "scheduler"，tokio-cron-scheduler）。
//! 通过 `Application::builder().cron_job(name, schedule, f)` 注册，启动时统一拉起。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::{AppError, AppResult};

/// 定时任务回调返回的 Future
pub type CronFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone)]
pub struct CronJob {
    pub name: String,
    pub schedule: String,
    /// 多实例部署时按 job name 加 redis 分布式锁，抢不到锁的实例本轮跳过
    pub distributed: bool,
    f: Arc<dyn Fn() -> CronFuture + Send + Sync>,
}

impl std::fmt::Debug for CronJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronJob")
            .field("name", &self.name)
            .field("schedule", &self.schedule)
            .field("distributed", &self.distributed)
            .finish()
    }
}

impl CronJob {
    pub fn new<F>(name: impl Into<String>, schedule: impl Into<String>, f: F) -> Self
    where
        F: Fn() -> CronFuture + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            schedule: schedule.into(),
            distributed: false,
            f: Arc::new(f),
        }
    }

    /// 开启分布式互斥（需 dist-lock feature 与 redis 缓存）。
    /// 锁 TTL 10 分钟：单轮执行超过 10 分钟的任务请自行用 `cache.try_lock` 控制。
    #[cfg(feature = "dist-lock")]
    #[must_use]
    pub fn distributed(mut self) -> Self {
        self.distributed = true;
        self
    }
}

/// 拉起调度器并注册全部任务，返回的句柄需保持存活（调用方持有到进程结束）。
/// schedule 为 6/7 段 cron（秒开头），如 `0/30 * * * * *`。
pub(crate) async fn start(
    jobs: Vec<CronJob>,
    cache: Option<&crate::cache::Cache>,
) -> AppResult<tokio_cron_scheduler::JobScheduler> {
    use tokio_cron_scheduler::JobScheduler;

    let scheduler = JobScheduler::new()
        .await
        .map_err(|e| AppError::internal(format!("scheduler init failed: {e}")))?;

    for job in jobs {
        let j = make_job(&job, cache)?;
        scheduler
            .add(j)
            .await
            .map_err(|e| AppError::internal(format!("cron job add failed: {e}")))?;
        tracing::info!(name = %job.name, schedule = %job.schedule, distributed = job.distributed, "cron job registered");
    }

    scheduler
        .start()
        .await
        .map_err(|e| AppError::internal(format!("scheduler start failed: {e}")))?;
    Ok(scheduler)
}

fn make_job(
    job: &CronJob,
    cache: Option<&crate::cache::Cache>,
) -> AppResult<tokio_cron_scheduler::Job> {
    use tokio_cron_scheduler::Job;

    #[cfg(feature = "dist-lock")]
    if job.distributed {
        let cache = cache.ok_or_else(|| {
            AppError::internal(format!(
                "cron job {:?} is distributed but redis cache is not configured",
                job.name
            ))
        })?;
        let f = job.f.clone();
        let cache = cache.clone();
        let name = job.name.clone();
        return Job::new_async(job.schedule.as_str(), move |_uuid, _sched| {
            let f = f.clone();
            let cache = cache.clone();
            let name = name.clone();
            Box::pin(async move {
                run_distributed(&name, &cache, f).await;
            })
        })
        .map_err(|e| AppError::internal(format!("cron job {:?} invalid: {e}", job.name)));
    }

    let f = job.f.clone();
    Job::new_async(job.schedule.as_str(), move |_uuid, _sched| f())
        .map_err(|e| AppError::internal(format!("cron job {:?} invalid: {e}", job.name)))
}

/// 分布式互斥执行：抢到锁的实例运行，其余实例本轮跳过。
/// 锁获取失败（redis 抖动）按跳过处理——宁可少跑一轮，不可多实例重复跑。
#[cfg(feature = "dist-lock")]
async fn run_distributed(
    name: &str,
    cache: &crate::cache::Cache,
    f: Arc<dyn Fn() -> CronFuture + Send + Sync>,
) {
    use std::time::Duration;

    match cache
        .try_lock(&format!("cron:lock:{name}"), Duration::from_secs(600))
        .await
    {
        Ok(Some(lock)) => {
            tracing::debug!(name, "cron job acquired distributed lock");
            f().await;
            let _ = lock.release().await;
        }
        Ok(None) => {
            tracing::debug!(name, "cron job skipped: another instance holds the lock")
        }
        Err(e) => {
            tracing::warn!(name, error = %e, "cron job skipped: distributed lock error")
        }
    }
}
