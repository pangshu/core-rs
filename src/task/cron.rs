//! cron 表达式调度（含时区），基于 tokio-cron-scheduler（6/7 段，秒开头）。
//! 时区用 IANA 名称（如 `Asia/Shanghai`）；解析失败回落 UTC 并告警。

use std::str::FromStr; // 引入 FromStr，用于把时区字符串解析为 Tz

use tokio_cron_scheduler::Job as TokioJob; // 引入底层调度 Job 类型并重命名避免与框架 Job 冲突

use crate::error::{AppError, AppResult}; // 引入框架错误类型与结果别名

/// 为任务构造 tokio-cron-scheduler Job；`distributed` 时经 cache/lock 选主。
#[allow(unused_variables)] // 允许未使用变量（非 distributed 分支不使用 state 等）
pub(crate) fn make_job<S>( // 构造底层调度 Job（crate 内可见）
    job: &crate::task::Job, // 框架任务定义
    schedule: &str, // cron 表达式
    timezone: &str, // IANA 时区名
    distributed: bool, // 是否启用多实例选主
    state: &S, // 应用状态（提供锁与配置）
) -> AppResult<TokioJob> // 返回构造好的底层 Job
where
    S: crate::traits::HasCache + crate::traits::HasConfig + Send + Sync + 'static, // 状态需提供缓存锁与配置
{
    let f = job.handler(); // 取出任务回调句柄

    // 多实例互斥：抢到锁的实例运行，其余实例本轮跳过。
    // 锁获取失败（redis 抖动）按跳过处理——宁可少跑一轮，不可多实例重复跑。
    if distributed { // 启用选主时走加锁分支
        let lock = state.lock().clone(); // 克隆分布式锁句柄进入闭包
        let lock_ttl = // 计算锁 TTL
            std::time::Duration::from_secs(state.config().load().task.lock_ttl_secs.max(60)); // 读配置并保证至少 60 秒
        let name = job.name.clone(); // 克隆任务名供闭包内构造锁键
        let wrapped = move || { // 包装回调：每轮先抢锁再执行
            let f = f.clone(); // 克隆回调句柄
            let lock = lock.clone(); // 克隆锁句柄
            let name = name.clone(); // 克隆任务名
            let lock_ttl = lock_ttl; // 复制 TTL（Duration 为 Copy）
            Box::pin(async move { // 返回装箱的异步任务
                match lock.try_acquire(&format!("cron:lock:{name}"), lock_ttl).await { // 尝试非阻塞获取分布式锁
                    Ok(Some(guard)) => { // 抢到锁：本实例执行
                        tracing::debug!(name, "cron job acquired distributed lock"); // 记录已获锁
                        f().await; // 执行真正的任务回调
                        let _ = guard.release().await; // 执行完释放锁（忽略释放错误）
                    }
                    Ok(None) => { // 未抢到锁：已有其他实例在跑
                        tracing::debug!(name, "cron job skipped: another instance holds the lock") // 记 debug 并跳过本轮
                    }
                    Err(e) => { // 锁后端出错（如 redis 抖动）
                        tracing::warn!(name, error = %e, "cron job skipped: distributed lock error") // 记 warning 并跳过本轮
                    }
                }
            }) as crate::task::job::JobFuture // 显式转换为框架的 JobFuture 类型
        };
        return with_timezone(schedule, timezone, move |_uuid, _sched| wrapped()); // 用带时区的方式构造 Job 并返回
    }

    with_timezone(schedule, timezone, move |_uuid, _sched| f()) // 非选主：直接包装回调并构造 Job
}

/// 按 IANA 时区名构造调度 Job（解析失败回落 UTC）
fn with_timezone<F>(schedule: &str, timezone: &str, f: F) -> AppResult<TokioJob> // 按时区构造底层 Job
where
    F: Fn(uuid_cron::Uuid, tokio_cron_scheduler::JobScheduler) -> crate::task::job::JobFuture // 回调签名与底层调度器一致
        + Send // 可跨线程发送
        + Sync // 可多线程共享
        + 'static, // 不借用短生命周期数据
{
    if timezone.is_empty() { // 未指定时区时
        return TokioJob::new_async(schedule, f) // 用无时区版本构造 Job
            .map_err(|e| AppError::internal(format!("cron job schedule {schedule:?} invalid: {e}"))); // 表达式非法则转内部错误
    }
    match chrono_tz::Tz::from_str(timezone) { // 把时区名解析为 chrono_tz 时区
        Ok(tz) => { // 解析成功
            #[allow(clippy::let_unit_value)] // 允许 let 绑定单元值（下面显式忽略返回）
            let _ = (); // 占位：显式丢弃单元值，避免未使用告警
            TokioJob::new_async_tz(schedule, tz, f).map_err(|e| { // 用指定时区构造 Job
                AppError::internal(format!( // 表达式非法时构造内部错误
                    "cron job schedule {schedule:?} invalid for tz {timezone}: {e}" // 错误消息含表达式与时区
                ))
            })
        }
        Err(_) => { // 时区名无法解析
            tracing::warn!(timezone, "unknown IANA timezone, falling back to UTC"); // 告警并回落 UTC
            TokioJob::new_async(schedule, f) // 用无时区（UTC）版本构造 Job
                .map_err(|e| AppError::internal(format!("cron job schedule {schedule:?} invalid: {e}"))) // 表达式非法转内部错误
        }
    }
}

/// tokio-cron-scheduler 的 uuid 再导出别名（Job 回调签名用，避免直接依赖其内部路径）
mod uuid_cron { // 私有模块：再导出 uuid 类型
    pub use uuid::Uuid; // 转发导出 uuid::Uuid 供回调签名使用
}
