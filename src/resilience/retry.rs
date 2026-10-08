//! 重试：指数退避 + 抖动（文档 三·7）。策略来自 `[resilience]` 配置。

use std::time::Duration; // 引入 Duration，表示退避时长

use crate::config::sections::ResiliencePolicy; // 引入弹性策略（重试次数/退避参数）
use crate::resilience::ResilienceError; // 引入弹性错误类型

/// 时长别名（retry 退避计算用）
pub type Backoff = Duration; // 把 Duration 别名为 Backoff，表达退避时长语义

/// 按策略重试包裹调用（重试期间每次都是全新 `f()` 调用）。
/// 所有错误一律重试——`max_retries` 放大请求量，上游返回 4xx 类明确拒绝时
/// 请改用 [`with_retry_when`] 只对可重试错误重试。
pub async fn with_retry<T, E, F, Fut>(policy: &ResiliencePolicy, f: F) -> Result<T, ResilienceError<E>> // 对所有错误一律重试
where
    T: Send, // 成功值可跨线程
    E: std::fmt::Display + Send, // 错误可打印且可跨线程
    F: Fn() -> Fut + Send + Sync, // 每次重试产出新的 Future
    Fut: std::future::Future<Output = Result<T, E>> + Send, // Future 可发送
{
    with_retry_when(policy, f, |_| true).await // 委托给可重试判定版，恒判定可重试
}

/// 带**可重试判定**的重试：`retryable` 返回 false 的错误立即失败、不消耗重试预算。
/// 典型用法：只对 5xx / 429 / 超时重试，4xx 客户端错误直接透传。
pub async fn with_retry_when<T, E, F, Fut, R>( // 带可重试判定的重试核心实现
    policy: &ResiliencePolicy, // 弹性策略
    f: F, // 被包裹的调用
    retryable: R, // 判定某错误是否值得重试
) -> Result<T, ResilienceError<E>> // 成功值或耗尽错误
where
    T: Send, // 成功值可跨线程
    E: std::fmt::Display + Send, // 错误可打印且可跨线程
    F: Fn() -> Fut + Send + Sync, // 每次重试产出新的 Future
    Fut: std::future::Future<Output = Result<T, E>> + Send, // Future 可发送
    R: Fn(&E) -> bool, // 判定闭包
{
    let rounds = policy.max_retries + 1; // 总尝试次数 = 重试次数 + 首次
    let mut attempt = 1u32; // 当前尝试序号，从 1 开始
    loop { // 循环直到成功或不再重试
        match f().await { // 执行一次调用
            Ok(value) => return Ok(value), // 成功：直接返回
            Err(e) if attempt < rounds && retryable(&e) => { // 还有预算且该错误可重试
                let backoff = backoff_for(policy, attempt); // 计算本次退避时长
                tracing::warn!(attempt, backoff_ms = backoff.as_millis() as u64, error = %e, "retrying"); // 记录即将重试
                tokio::time::sleep(backoff).await; // 退避等待
                attempt += 1; // 尝试序号 +1
            }
            Err(e) => { // 预算耗尽或错误不可重试
                return Err(ResilienceError::Exhausted { // 返回重试耗尽错误
                    attempts: attempt, // 实际尝试次数
                    source: e, // 底层错误
                });
            }
        }
    }
}

/// 指数退避 + 抖动：base * 2^(attempt-1)，上限 backoff_max_ms，加 0~10% 抖动
pub fn backoff_for(policy: &ResiliencePolicy, attempt: u32) -> Duration { // 计算第 attempt 次尝试的退避时长
    let base = Duration::from_millis(policy.backoff_ms.max(1)); // 基准退避（至少 1ms）
    let shift = attempt.saturating_sub(1).min(20); // 指数位移量，限制上限避免溢出
    let exp = base.saturating_mul(1u32.wrapping_shl(shift).max(1)); // base * 2^shift（饱和乘）
    let capped = exp.min(Duration::from_millis(policy.backoff_max_ms.max(1))); // 受最大退避上限约束
    let nanos = std::time::SystemTime::now() // 取当前系统时间
        .duration_since(std::time::UNIX_EPOCH) // 换算为自 UNIX 纪元以来的时长
        .map(|t| t.subsec_nanos() as u64) // 取出亚秒纳秒部分作为抖动源
        .unwrap_or(0); // 时间早于纪元时退化为 0
    let jitter = capped.as_nanos() as u64 * (nanos % 100) / 100; // 取 0~10% 的随机抖动
    capped + Duration::from_nanos(jitter) // 退避时长加上抖动
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let p = ResiliencePolicy { backoff_ms: 100, backoff_max_ms: 1000, ..Default::default() };
        assert!(backoff_for(&p, 1).as_millis() < 120);
        assert!(backoff_for(&p, 2).as_millis() < 240);
        assert!(backoff_for(&p, 3).as_millis() < 440);
        assert!(backoff_for(&p, 30).as_millis() <= 1100);
    }
}
