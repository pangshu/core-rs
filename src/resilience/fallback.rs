//! 降级（fallback）。
//!
//! 主调用失败（依赖不可用）时返回兜底值，避免把故障放大到上游。
//!
//! # 只降级「依赖故障」
//!
//! 降级的判定依据是 [`is_degradable`]：依赖故障（5xx 类）+ 自身拒绝（熔断打开 /
//! 舱壁已满）+ 本进程未就绪。**认证、授权、限流、参数校验、业务错误一律不降级**，
//! 原样传播给调用方。
//!
//! 这不是可选的保守设定：兜底值天然是「默认值」，把 `401`/`403` 兜底成默认值
//! 就等于 fail-open——上游鉴权服务抖一下，全部请求就按「有权限」处理了。
//! 这与 `docs/03` 的「不静默降级」一致。

use std::future::Future;

use crate::errors::AppResult;
use crate::resilience::circuit_breaker::is_degradable;

/// 固定兜底值的降级器。
#[derive(Debug, Clone)]
pub struct Fallback<T> {
    value: T,
}

impl<T> Fallback<T> {
    /// 以兜底值创建。
    #[must_use]
    pub fn new(value: T) -> Self {
        Self { value }
    }

    /// 执行主调用；**依赖故障**时返回兜底值（并记 warn）。
    ///
    /// 兜底值需要 `Clone`（可能被多次使用）。
    ///
    /// # Errors
    /// 非依赖故障（4xx 业务错误、认证 / 授权 / 限流）原样返回，不降级。
    pub async fn run<P, PF>(&self, primary: P) -> AppResult<T>
    where
        T: Clone,
        P: FnOnce() -> PF,
        PF: Future<Output = AppResult<T>>,
    {
        match primary().await {
            Ok(value) => Ok(value),
            Err(error) if is_degradable(error.code()) => {
                tracing::warn!(
                    code = %error.code(),
                    error = ?error,
                    "主调用失败（依赖故障），返回兜底值"
                );
                Ok(self.value.clone())
            }
            Err(error) => {
                tracing::warn!(
                    code = %error.code(),
                    "主调用失败但不是依赖故障，不降级（fail-closed）"
                );
                Err(error)
            }
        }
    }
}

/// 函数式降级：**依赖故障**时执行 `fallback`。
///
/// # Errors
/// - 主调用返回非依赖故障时原样返回（不降级）
/// - `fallback` 自身的错误原样返回
pub async fn with_fallback<T, P, PF, F, FF>(primary: P, fallback: F) -> AppResult<T>
where
    P: FnOnce() -> PF,
    PF: Future<Output = AppResult<T>>,
    F: FnOnce() -> FF,
    FF: Future<Output = AppResult<T>>,
{
    match primary().await {
        Ok(value) => Ok(value),
        Err(error) if is_degradable(error.code()) => {
            tracing::warn!(
                code = %error.code(),
                error = ?error,
                "主调用失败（依赖故障），执行降级逻辑"
            );
            fallback().await
        }
        Err(error) => {
            tracing::warn!(
                code = %error.code(),
                "主调用失败但不是依赖故障，不降级（fail-closed）"
            );
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{AppError, ErrorCode};

    #[tokio::test]
    async fn should_return_primary_value_when_ok() {
        let fallback = Fallback::new(0);
        let value = fallback
            .run(|| async { Ok::<_, AppError>(42) })
            .await
            .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn should_return_fallback_when_primary_fails() {
        let fallback = Fallback::new(-1);
        let value = fallback
            .run(|| async { Err::<i32, _>(AppError::internal(anyhow::anyhow!("down"))) })
            .await
            .unwrap();
        assert_eq!(value, -1);
    }

    #[tokio::test]
    async fn should_degrade_when_circuit_open() {
        let fallback = Fallback::new("cached");
        let value = fallback
            .run(|| async { Err::<&str, _>(AppError::biz(ErrorCode::CIRCUIT_OPEN, "熔断中")) })
            .await
            .unwrap();
        assert_eq!(value, "cached", "熔断打开属于依赖不可用，应降级");
    }

    #[tokio::test]
    async fn should_not_degrade_auth_errors() {
        let fallback = Fallback::new(true);
        for code in [
            ErrorCode::UNAUTHENTICATED,
            ErrorCode::TOKEN_EXPIRED,
            ErrorCode::FORBIDDEN,
            ErrorCode::PERMISSION_DENIED,
        ] {
            let error = fallback
                .run(move || async move { Err::<bool, _>(AppError::biz(code, "denied")) })
                .await
                .unwrap_err();
            assert_eq!(error.code(), code, "鉴权错误不得降级（否则 fail-open）");
        }
    }

    #[tokio::test]
    async fn should_not_degrade_business_errors() {
        let fallback = Fallback::new(0);
        for code in [
            ErrorCode::INVALID_PARAM,
            ErrorCode::NOT_FOUND,
            ErrorCode::RATE_LIMITED,
            ErrorCode::IP_BLOCKED,
        ] {
            let error = fallback
                .run(move || async move { Err::<i32, _>(AppError::biz(code, "nope")) })
                .await
                .unwrap_err();
            assert_eq!(error.code(), code);
        }
    }

    #[tokio::test]
    async fn should_run_fallback_fn_when_primary_fails() {
        let value = with_fallback(
            || async { Err::<i32, _>(AppError::biz(ErrorCode::UPSTREAM_ERROR, "502")) },
            || async { Ok::<_, AppError>(7) },
        )
        .await
        .unwrap();
        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn should_propagate_primary_error_when_not_degradable() {
        let error = with_fallback(
            || async { Err::<i32, _>(AppError::forbidden("no")) },
            || async { Ok::<_, AppError>(7) },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::FORBIDDEN, "降级逻辑不应执行");
    }

    #[tokio::test]
    async fn should_propagate_fallback_error() {
        let error = with_fallback(
            || async { Err::<i32, _>(AppError::internal(anyhow::anyhow!("a"))) },
            || async { Err::<i32, _>(AppError::biz(ErrorCode::DB_ERROR, "b")) },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DB_ERROR);
    }
}
