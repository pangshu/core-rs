//! 出站调用重试（`[resilience].retry`）。
//!
//! 与 `[queue].retry` 语义不同：这里是**出站调用**重试，队列那侧是**消费失败**重试
//! （见 `docs/08` §3）。
//!
//! # 什么该重试
//!
//! 只重试「确定失败」的依赖故障（5xx 类，见 [`is_retryable`]）。两类例外：
//!
//! - **超时**（`10006`）：结果未知，不是确定失败。下游可能已经执行完，重试就是
//!   重复执行。仅当调用被标记为**幂等**（[`RetryPolicy::with_idempotent`]）时才重试。
//! - **自身拒绝**（`50006` / `50007`）：重试只会把拒绝放大，跨服务调用链上还会
//!   让上游把下游的自我保护记成自己的失败率（级联熔断）。

use std::future::Future;
use std::time::Duration;

use crate::config::ResilienceSection;
use crate::config::sections::RetryBackoff;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::resilience::circuit_breaker::is_retryable;

/// 重试策略。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff: RetryBackoff,
    base_delay: Duration,
    max_delay: Duration,
    idempotent: bool,
}

impl RetryPolicy {
    /// 直接指定参数创建（**非幂等**语义，不重试超时）。
    #[must_use]
    pub fn new(
        max_attempts: u32,
        backoff: RetryBackoff,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff,
            base_delay,
            max_delay,
            idempotent: false,
        }
    }

    /// 由 `[resilience]` 配置构造。
    #[must_use]
    pub fn from_config(config: &ResilienceSection) -> Self {
        Self::new(
            config.retry.max_attempts,
            config.retry.backoff,
            config.retry.base_delay,
            config.retry.max_delay,
        )
        .with_idempotent(config.retry.idempotent)
    }

    /// 声明被重试的调用是否**幂等**（默认 `false`）。
    ///
    /// 幂等时超时也重试；非幂等时超时立即返回——否则下游可能已经执行完，
    /// 重试会造成重复扣款 / 重复下单。
    #[must_use]
    pub fn with_idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// 最大尝试次数（含首次）。
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// 是否按幂等调用处理。
    #[must_use]
    pub fn is_idempotent(&self) -> bool {
        self.idempotent
    }

    /// 第 `attempt` 次尝试失败后是否应再试一次。
    ///
    /// `attempt` 从 1 开始（首次尝试失败时传 1）。
    #[must_use]
    pub fn should_retry(&self, error: &AppError, attempt: u32) -> bool {
        if attempt >= self.max_attempts {
            return false;
        }
        let code = error.code();
        if !is_retryable(code) {
            return false;
        }
        // 超时 = 结果未知：非幂等调用重试会重复执行
        if code == ErrorCode::GATEWAY_TIMEOUT && !self.idempotent {
            return false;
        }
        true
    }

    /// 执行操作；仅对可重试错误重试，4xx 业务错误立即返回。
    ///
    /// # Errors
    /// 重试耗尽或遇到不可重试错误时，返回最后一次的错误。
    pub async fn call<F, Fut, T>(&self, mut operation: F) -> AppResult<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let mut attempt = 1;
        loop {
            match operation().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !self.should_retry(&error, attempt) {
                        return Err(error);
                    }
                    let delay = self.delay_for(attempt);
                    tracing::warn!(
                        attempt,
                        max_attempts = self.max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        code = %error.code(),
                        "出站调用失败，准备重试"
                    );
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// 第 `attempt` 次失败后的退避时长。
    pub(crate) fn delay_for(&self, attempt: u32) -> Duration {
        let delay = match self.backoff {
            RetryBackoff::Fixed => self.base_delay,
            RetryBackoff::Exponential => {
                let exponent = attempt.saturating_sub(1).min(16);
                self.base_delay.saturating_mul(1u32 << exponent.min(16))
            }
        };
        delay.min(self.max_delay)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy::new(
            max_attempts,
            RetryBackoff::Fixed,
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
    }

    #[tokio::test]
    async fn should_return_immediately_on_success() {
        let calls = AtomicU32::new(0);
        let value = policy(3)
            .call(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, AppError>(7)
            })
            .await
            .unwrap();

        assert_eq!(value, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn should_retry_until_success() {
        let calls = AtomicU32::new(0);
        let value = policy(3)
            .call(|| async {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(AppError::internal(anyhow::anyhow!("transient")))
                } else {
                    Ok(9)
                }
            })
            .await
            .unwrap();

        assert_eq!(value, 9);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn should_stop_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let error = policy(2)
            .call(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(AppError::internal(anyhow::anyhow!("always")))
            })
            .await
            .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(error.code(), ErrorCode::INTERNAL);
    }

    #[tokio::test]
    async fn should_not_retry_business_errors() {
        let calls = AtomicU32::new(0);
        let error = policy(5)
            .call(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(AppError::not_found("user"))
            })
            .await
            .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx 不应重试");
        assert_eq!(error.code(), ErrorCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn should_not_retry_timeout_when_not_idempotent() {
        let calls = AtomicU32::new(0);
        let error = policy(3)
            .call(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(AppError::biz(ErrorCode::GATEWAY_TIMEOUT, "timeout"))
            })
            .await
            .unwrap_err();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "非幂等调用的超时结果未知，不得重试（否则重复执行）"
        );
        assert_eq!(error.code(), ErrorCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn should_retry_timeout_when_idempotent() {
        let calls = AtomicU32::new(0);
        let error = policy(3)
            .with_idempotent(true)
            .call(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(AppError::biz(ErrorCode::GATEWAY_TIMEOUT, "timeout"))
            })
            .await
            .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 3, "幂等调用可重试超时");
        assert_eq!(error.code(), ErrorCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn should_not_retry_self_rejection() {
        async fn attempts_for(code: ErrorCode) -> u32 {
            let calls = AtomicU32::new(0);
            let calls_ref = &calls;
            let _ = policy(5)
                .with_idempotent(true)
                .call(move || async move {
                    calls_ref.fetch_add(1, Ordering::SeqCst);
                    Err::<i32, _>(AppError::biz(code, "rejected"))
                })
                .await;
            calls.load(Ordering::SeqCst)
        }

        assert_eq!(
            attempts_for(ErrorCode::CIRCUIT_OPEN).await,
            1,
            "自身拒绝不得重试"
        );
        assert_eq!(
            attempts_for(ErrorCode::BULKHEAD_FULL).await,
            1,
            "自身拒绝不得重试"
        );
    }

    #[test]
    fn should_cap_exponential_delay() {
        let policy = RetryPolicy::new(
            10,
            RetryBackoff::Exponential,
            Duration::from_millis(100),
            Duration::from_millis(500),
        );
        assert_eq!(policy.delay_for(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for(9), Duration::from_millis(500), "应封顶");
    }
}
