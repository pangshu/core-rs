//! 降级（文档 三·7）：兜底值 / 兜底响应 / 缓存兜底。与 `fallback_enabled`
//! 配置开关配合——业务侧按需选用，框架不强制包裹所有调用。

use std::future::Future; // 引入 Future trait，用于约束入参

use crate::resilience::ResilienceError; // 引入弹性错误类型，统一失败语义

/// 调用失败（或被熔断拒绝）时使用兜底值
pub async fn or_value<T, E, F, Fut>(fut: F, fallback: T) -> Result<T, ResilienceError<E>> // 失败则返回预置兜底值
where
    F: Future<Output = Result<T, ResilienceError<E>>>, // 被包裹的弹性调用
    E: std::fmt::Display, // 底层错误需可打印
{
    match fut.await { // 等待被包裹调用结果
        Ok(v) => Ok(v), // 成功：原样返回
        Err(e) => { // 失败（含熔断拒绝）
            tracing::warn!(error = %e, "falling back to default value"); // 记 warning 便于观测降级
            Ok(fallback) // 返回兜底值
        }
    }
}

/// 调用失败时执行兜底闭包（如读缓存兜底）
pub async fn or_with<T, E, F, Fut, G, GFut>(fut: F, fallback: G) -> Result<T, ResilienceError<E>> // 失败则执行兜底闭包
where
    F: Future<Output = Result<T, ResilienceError<E>>>, // 被包裹的弹性调用
    E: std::fmt::Display, // 底层错误需可打印
    G: FnOnce() -> GFut, // 兜底闭包，只调用一次
    GFut: Future<Output = T>, // 兜底闭包产出的 Future
{
    match fut.await { // 等待被包裹调用结果
        Ok(v) => Ok(v), // 成功：原样返回
        Err(e) => { // 失败（含熔断拒绝）
            tracing::warn!(error = %e, "falling back to fallback closure"); // 记 warning 便于观测降级
            Ok(fallback().await) // 执行兜底闭包并返回其结果
        }
    }
}
