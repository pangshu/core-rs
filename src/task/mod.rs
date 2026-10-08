//! 任务调度（文档 三·19）：定时任务（cron）。与 `queue/` 分工——queue 管
//! **异步消息投递**，task 管**周期调度**；二者可组合（定时任务把活投给队列执行）。
//!
//! - 注册：`App::task(Job::new("cleanup", "0 0 3 * * *", handler))`；
//! - 多实例防重复：`[task].distributed_lock = true` 时经 `cache/lock` 选主执行，
//!   保证同一时刻仅一个实例运行（对标 Celery beat / @Scheduled 的集群语义）；
//! - feature = "scheduler"（tokio-cron-scheduler，6/7 段秒开头 cron 表达式）。

#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
pub mod cron; // 声明 cron 子模块：cron 表达式调度实现
#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
pub mod distributed; // 声明 distributed 子模块：多实例选主助手
#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
pub mod job; // 声明 job 子模块：任务定义与回调类型

#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
mod start; // 按配置注册并启动 cron 调度器

#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
pub use job::{Job, JobFuture}; // 对外导出任务类型 Job 与回调 Future 别名
#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
pub(crate) use start::start_from_config; // 供 App::serve 按配置启动调度器
