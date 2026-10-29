//! 舱壁隔离（`[resilience].bulkhead`）与**出站**超时（`[resilience].timeout`）。
//!
//! 舱壁限制并发出站调用数，超限**快速拒绝**（`50007`）而不是排队膨胀；
//! 出站超时独立于入站 `[server].request_timeout`（见 `docs/08` §3）。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::config::ResilienceSection;
use crate::errors::{AppError, AppResult, ErrorCode};

/// 并发舱壁。
#[derive(Debug, Clone)]
pub struct Bulkhead {
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
}

impl Bulkhead {
    /// 以并发上限创建。
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.max(1);
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        }
    }

    /// 由 `[resilience]` 配置创建。
    #[must_use]
    pub fn from_config(config: &ResilienceSection) -> Self {
        Self::new(config.bulkhead.max_concurrent as usize)
    }

    /// 并发上限。
    #[must_use]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// 当前可用名额。
    #[must_use]
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// 尝试占用一个并发名额（供 [`PolicyRegistry`](crate::resilience::PolicyRegistry) 组合使用）。
    ///
    /// 返回的 permit 在 drop 时归还名额。
    ///
    /// # Errors
    /// 名额耗尽时返回 `50007`（`BULKHEAD_FULL`）——**本进程自身的拒绝**，
    /// 调用方不应据此重试，也不应计入上游熔断。
    pub fn try_acquire(&self) -> AppResult<tokio::sync::OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().map_err(|_| {
            AppError::biz(
                ErrorCode::BULKHEAD_FULL,
                "并发舱壁已满，拒绝请求".to_string(),
            )
        })
    }

    /// 在舱壁内执行一次出站调用；名额耗尽时**快速拒绝**。
    ///
    /// # Errors
    /// - 名额耗尽 → `50007`（`BULKHEAD_FULL`）
    /// - 调用本身失败时原样返回
    pub async fn call<F, Fut, T>(&self, operation: F) -> AppResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = AppResult<T>>,
    {
        let permit = self.try_acquire()?;

        let result = operation().await;
        drop(permit);
        result
    }
}

/// 给出站调用套超时；超时映射为 `10006`（`GATEWAY_TIMEOUT`）。
///
/// `timeout` 为零时**不套超时**（直接透传）。这是显式逃生口，不是配置开关：
/// `[resilience].timeout.default = 0` 会在启动期被
/// [`ResilienceSection::validate`](crate::config::ResilienceSection::validate) 拒绝，
/// 免得「配了超时」实际等于没有。
///
/// # Errors
/// 超时 → `10006`；调用失败原样返回。
pub async fn with_timeout<T, F>(timeout: Duration, operation: F) -> AppResult<T>
where
    F: std::future::Future<Output = AppResult<T>>,
{
    if timeout.is_zero() {
        return operation.await;
    }
    match tokio::time::timeout(timeout, operation).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(timeout_ms = timeout.as_millis() as u64, "出站调用超时");
            Err(AppError::biz(ErrorCode::GATEWAY_TIMEOUT, "上游调用超时"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_run_within_limit() {
        let bulkhead = Bulkhead::new(2);
        let value = bulkhead
            .call(|| async { Ok::<_, AppError>(1) })
            .await
            .unwrap();
        assert_eq!(value, 1);
        assert_eq!(bulkhead.available(), 2, "调用结束应归还名额");
    }

    #[tokio::test]
    async fn should_reject_when_saturated() {
        let bulkhead = Bulkhead::new(1);

        let held = bulkhead
            .call(|| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<_, AppError>(())
            })
            .await;

        // 先占住唯一名额，再尝试进入
        let inner = bulkhead.clone();
        let task = tokio::spawn(async move {
            inner
                .call(|| async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Ok::<_, AppError>(2)
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        let error = bulkhead
            .call(|| async { Ok::<_, AppError>(3) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BULKHEAD_FULL);
        assert_eq!(error.code().status(), http::StatusCode::SERVICE_UNAVAILABLE);

        assert!(held.is_ok());
        assert_eq!(task.await.unwrap().unwrap(), 2);
    }

    #[tokio::test]
    async fn should_return_permit_when_call_is_cancelled() {
        let bulkhead = Bulkhead::new(1);

        let inner = bulkhead.clone();
        let handle = tokio::spawn(async move {
            inner
                .call(|| async { std::future::pending::<AppResult<()>>().await })
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(bulkhead.available(), 0, "名额应被占住");

        handle.abort();
        let _ = handle.await;

        assert_eq!(bulkhead.available(), 1, "取消后名额必须归还");
    }

    #[tokio::test]
    async fn should_map_timeout_to_gateway_timeout() {
        let error = with_timeout(Duration::from_millis(10), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, AppError>(())
        })
        .await
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::GATEWAY_TIMEOUT);
        assert_eq!(error.code().status(), http::StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn should_pass_through_when_fast_enough() {
        let value = with_timeout(Duration::from_secs(1), async { Ok::<_, AppError>(5) })
            .await
            .unwrap();
        assert_eq!(value, 5);
    }
}
