//! 降级（文档 三·7）：兜底值 / 兜底响应 / 缓存兜底。与 `fallback_enabled`
//! 配置开关配合——业务侧按需选用，框架不强制包裹所有调用。

use std::future::Future;

use crate::resilience::ResilienceError;

/// 调用失败（或被熔断拒绝）时使用兜底值
pub async fn or_value<T, E, F, Fut>(fut: F, fallback: T) -> Result<T, ResilienceError<E>>
where
    F: Future<Output = Result<T, ResilienceError<E>>>,
    E: std::fmt::Display,
{
    match fut.await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(error = %e, "falling back to default value");
            Ok(fallback)
        }
    }
}

/// 调用失败时执行兜底闭包（如读缓存兜底）
pub async fn or_with<T, E, F, Fut, G, GFut>(fut: F, fallback: G) -> Result<T, ResilienceError<E>>
where
    F: Future<Output = Result<T, ResilienceError<E>>>,
    E: std::fmt::Display,
    G: FnOnce() -> GFut,
    GFut: Future<Output = T>,
{
    match fut.await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(error = %e, "falling back to fallback closure");
            Ok(fallback().await)
        }
    }
}
