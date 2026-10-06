//! 重试：指数退避 + 抖动（文档 三·7）。策略来自 `[resilience]` 配置。

use std::time::Duration;

use crate::config::sections::ResiliencePolicy;
use crate::resilience::ResilienceError;

/// 按策略重试包裹调用（重试期间每次都是全新 `f()` 调用）。
/// 所有错误一律重试——`max_retries` 放大请求量，上游返回 4xx 类明确拒绝时
/// 请改用 [`with_retry_when`] 只对可重试错误重试。
pub async fn with_retry<T, E, F, Fut>(policy: &ResiliencePolicy, f: F) -> Result<T, ResilienceError<E>>
where
    T: Send,
    E: std::fmt::Display + Send,
    F: Fn() -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<T, E>> + Send,
{
    with_retry_when(policy, f, |_| true).await
}

/// 带**可重试判定**的重试：`retryable` 返回 false 的错误立即失败、不消耗重试预算。
/// 典型用法：只对 5xx / 429 / 超时重试，4xx 客户端错误直接透传。
pub async fn with_retry_when<T, E, F, Fut, R>(
    policy: &ResiliencePolicy,
    f: F,
    retryable: R,
) -> Result<T, ResilienceError<E>>
where
    T: Send,
    E: std::fmt::Display + Send,
    F: Fn() -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<T, E>> + Send,
    R: Fn(&E) -> bool,
{
    let rounds = policy.max_retries + 1;
    let mut attempt = 1u32;
    loop {
        match f().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt < rounds && retryable(&e) => {
                let backoff = backoff_for(policy, attempt);
                tracing::warn!(attempt, backoff_ms = backoff.as_millis() as u64, error = %e, "retrying");
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(e) => {
                return Err(ResilienceError::Exhausted {
                    attempts: attempt,
                    source: e,
                });
            }
        }
    }
}

/// 指数退避 + 抖动：base * 2^(attempt-1)，上限 backoff_max_ms，加 0~10% 抖动
pub fn backoff_for(policy: &ResiliencePolicy, attempt: u32) -> Duration {
    let base = Duration::from_millis(policy.backoff_ms.max(1));
    let shift = attempt.saturating_sub(1).min(20);
    let exp = base.saturating_mul(1u32.wrapping_shl(shift).max(1));
    let capped = exp.min(Duration::from_millis(policy.backoff_max_ms.max(1)));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = capped.as_nanos() as u64 * (nanos % 100) / 100;
    capped + Duration::from_nanos(jitter)
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
