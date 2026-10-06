//! Job 定义（文档 三·19）：名称、表达式、handler；超时与重试由 handler 内部
//! 自行处理（调度器只负责触发；长任务建议投递到 queue 执行）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 定时任务回调返回的 Future
pub type JobFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// 任务回调：拿到 handler 自身闭包（多实例互斥由框架层包在闭包外）
pub type JobHandler = Arc<dyn Fn() -> JobFuture + Send + Sync>;

#[derive(Clone)]
pub struct Job {
    /// 任务名（与 `[task].jobs` 配置的 name 对应）
    pub name: String,
    f: JobHandler,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("name", &self.name).finish()
    }
}

impl Job {
    pub fn new<F>(name: impl Into<String>, f: F) -> Self
    where
        F: Fn() -> JobFuture + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            f: Arc::new(f),
        }
    }

    /// 从 async 闭包快捷构造
    pub fn async_fn<F, Fut>(name: impl Into<String>, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::new(name, move || Box::pin(f()))
    }

    pub(crate) fn handler(&self) -> JobHandler {
        self.f.clone()
    }
}
