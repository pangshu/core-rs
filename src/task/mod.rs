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
pub use job::{Job, JobFuture}; // 对外导出任务类型 Job 与回调 Future 别名


#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
use crate::error::{AppError, AppResult}; // 引入框架错误类型，用于把调度器错误统一包装
#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译下面这行
use crate::traits::{HasCache, HasConfig}; // 引入状态能力 trait，约束 S 能提供缓存锁与配置

/// 注册任务清单并启动调度器（App::serve 内部调用）。
/// `[task].jobs` 配置声明启用的任务（按 name 匹配 handler）；
/// 空 jobs 列表 = 全部未启用，不启动调度器。
#[cfg(feature = "scheduler")] // 仅在开启 scheduler feature 时编译本函数
pub(crate) async fn start_from_config<S>( // 异步启动入口：按配置筛选并注册 cron 任务
    jobs: Vec<Job>, // 代码中注册的全部候选任务
    state: &S, // 应用状态引用，用于读配置与抢分布式锁
) -> AppResult<Option<tokio_cron_scheduler::JobScheduler>> // 返回调度器；未启用任何任务时为 None
where
    S: HasCache + HasConfig + Send + Sync + 'static, // 约束状态需能提供缓存锁与配置且可跨线程
{
    let settings = state.config().load().task.clone(); // 无锁读配置并克隆出 [task] 段设置
    if !settings.enabled || jobs.is_empty() { // 总开关关闭或没有任何候选任务时
        return Ok(None); // 直接返回 None，不创建也不启动调度器
    }

    let scheduler = tokio_cron_scheduler::JobScheduler::new() // 创建底层 tokio-cron-scheduler 调度器
        .await // 等待异步创建完成
        .map_err(|e| AppError::internal(format!("scheduler init failed: {e}")))?; // 初始化失败转为框架内部错误返回

    let mut enabled_count = 0; // 统计实际注册成功的任务数
    for job in &jobs { // 遍历代码注册的每个候选任务
        // 配置声明启用的任务才注册；配置里有声明但代码未注册的记 warning
        let Some(decl) = settings.jobs.iter().find(|d| d.name == job.name) else { // 按 name 在配置中查找声明
            tracing::debug!(name = %job.name, "cron job not enabled in [task].jobs config"); // 未在配置声明，记 debug 并跳过
            continue; // 跳过该任务，继续下一个
        };
        let schedule = decl.schedule.clone(); // 取出该任务的 cron 表达式
        let distributed = settings.distributed_lock; // 是否启用多实例选主（全局开关）
        let tz = decl.timezone.clone(); // 取出该任务的 IANA 时区名

        let j = cron::make_job(job, &schedule, &tz, distributed, state)?; // 构造底层调度 Job（含选主包装）
        scheduler // 取得调度器
            .add(j) // 把构造好的 Job 加入调度器
            .await // 等待异步加入完成
            .map_err(|e| AppError::internal(format!("cron job {:?} add failed: {e}", job.name)))?; // 加入失败转内部错误
        tracing::info!( // 注册成功记 info 日志
            name = %job.name, // 记录任务名
            schedule = %schedule, // 记录 cron 表达式
            timezone = %tz, // 记录时区
            distributed, // 记录是否启用分布式选主
            "cron job registered" // 日志消息文本
        );
        enabled_count += 1; // 注册成功计数 +1
    }

    // 配置声明了但代码没注册的，启动期提示（拼写错误检查）
    for decl in &settings.jobs { // 遍历配置中声明的所有任务
        if !jobs.iter().any(|j| j.name == decl.name) { // 若代码里没有同名 handler
            tracing::warn!( // 记 warning 提示拼写错误或漏注册
                name = %decl.name, // 记录未注册的任务名
                "cron job declared in [task].jobs but no handler registered" // 日志消息文本
            );
        }
    }

    if enabled_count == 0 { // 一个任务都没注册成功时
        return Ok(None); // 不启动调度器，返回 None
    }

    scheduler // 取得调度器
        .start() // 启动调度循环
        .await // 等待启动完成
        .map_err(|e| AppError::internal(format!("scheduler start failed: {e}")))?; // 启动失败转内部错误
    Ok(Some(scheduler)) // 返回已启动的调度器，交由 App 持有
}
