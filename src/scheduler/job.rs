//! 任务抽象。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;

use crate::errors::AppResult;

/// 任务执行体返回的 boxed future（避免引入 `futures-util`）。
pub(crate) type JobFuture = Pin<Box<dyn Future<Output = AppResult<()>> + Send + 'static>>;

/// 闭包形态的任务处理器。
pub(crate) type JobHandler = std::sync::Arc<dyn Fn() -> JobFuture + Send + Sync + 'static>;

/// 可被调度的任务。
///
/// 一般直接用 [`register_cron`](crate::scheduler::Scheduler::register_cron) /
/// [`register_once`](crate::scheduler::Scheduler::register_once) 传闭包即可；
/// 需要**自定义任务名**（选主锁键）时实现本 trait 后走
/// [`register`](crate::scheduler::Scheduler::register)。
#[async_trait]
pub trait Job: Send + Sync + 'static {
    /// 任务名（用于日志与分布式选主锁键，需在应用内唯一）。
    fn name(&self) -> &str;

    /// 执行一次。
    ///
    /// # Errors
    /// 任务失败时返回错误（只记日志，不影响后续调度）。
    async fn run(&self) -> AppResult<()>;
}

/// 调度规格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobSpec {
    /// cron 表达式（**6 段，含秒**：`秒 分 时 日 月 周`，如 `"0 0 3 * * *"`）。
    Cron(String),
    /// 一次性延迟任务（进程启动后延迟多久触发一次）。
    Once(Duration),
}

impl JobSpec {
    /// 用于日志/锁键的简短描述。
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Cron(expr) => format!("cron:{expr}"),
            Self::Once(delay) => format!("once:{}ms", delay.as_millis()),
        }
    }
}

/// 闭包任务适配器。
pub(crate) struct FnJob {
    name: String,
    handler: JobHandler,
}

impl FnJob {
    pub(crate) fn new(name: impl Into<String>, handler: JobHandler) -> Self {
        Self {
            name: name.into(),
            handler,
        }
    }
}

#[async_trait]
impl Job for FnJob {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self) -> AppResult<()> {
        (self.handler)().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::AppError;

    #[test]
    fn should_describe_specs() {
        assert_eq!(
            JobSpec::Cron("0 0 3 * * *".into()).describe(),
            "cron:0 0 3 * * *"
        );
        assert_eq!(
            JobSpec::Once(Duration::from_millis(1500)).describe(),
            "once:1500ms"
        );
    }

    #[tokio::test]
    async fn should_run_fn_job() {
        let job = FnJob::new("t", std::sync::Arc::new(|| Box::pin(async { Ok(()) })));
        assert_eq!(job.name(), "t");
        assert!(job.run().await.is_ok());
    }

    #[tokio::test]
    async fn should_propagate_job_error() {
        let job = FnJob::new(
            "t",
            std::sync::Arc::new(|| {
                Box::pin(async { Err(AppError::biz(crate::errors::ErrorCode::INTERNAL, "boom")) })
            }),
        );
        assert!(job.run().await.is_err());
    }
}
