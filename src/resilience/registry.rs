//! 出站治理策略注册表（`[resilience]` 的统一入口）。
//!
//! 一次性从配置构建三类策略（重试 / 熔断 / 舱壁）与出站超时，供业务侧按依赖
//! 复用。两种用法：
//!
//! - [`PolicyRegistry::execute`]：所有调用共用一套熔断器与舱壁（**默认**，向后兼容）
//! - [`PolicyRegistry::execute_scoped`]：按依赖名各自一套（**推荐**）
//!
//! # 为什么推荐按依赖隔离
//!
//! 共用一套舱壁时，一个慢依赖就能把全部出站名额占满——这是「共享队列」而不是
//! 「舱壁」；共用一套熔断器时，支付网关挂了会把用户中心的调用一起熔断。
//! 只有按依赖分桶，隔离才成立。
//!
//! # 治理顺序
//!
//! 每次**尝试**都完整走一遍：舱壁 → 熔断 → 超时，失败后按重试策略退避。
//!
//! ```text
//! for attempt in 1..=max_attempts {
//!     舱壁占名额 → 熔断准入 → 超时包裹的一次调用 → 归还名额 → 记账
//!     失败且可重试 → 退避（不占名额）
//! }
//! ```
//!
//! 两点和旧实现不同，都是刻意的：
//!
//! 1. **熔断器按「每次尝试」计数**。旧实现把整个重试序列记成 1 个样本，
//!    于是 `failure_threshold = 5` 实际要 15 次下游失败才打开（`max_attempts = 3`），
//!    而重试只要成功一次就把计数清零——对「一半请求失败」的依赖永不熔断。
//! 2. **舱壁名额不跨退避持有**。旧实现整个函数持有一个 permit，退避 sleep 也占着，
//!    100 个名额 × 3 次 × 10s 超时足以让全部出站停摆。
//!
//! **启动期快照**：策略在 `AppState::build` 时按 `[resilience]` 一次性构建，
//! 改配置**需重启**（`docs/08` §2.9）。要支持热更新，得把这里改成持 `ArcSwap`
//! 并在每次 `execute` 时读快照。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::ResilienceSection;
use crate::errors::AppResult;
use crate::resilience::bulkhead::{self, Bulkhead};
use crate::resilience::circuit_breaker::CircuitBreaker;
use crate::resilience::retry::RetryPolicy;

/// 按依赖名缓存的策略条目上限。
///
/// 依赖名应当是代码里的常量。设上限是为了防止「名字来自请求」这种误用把
/// 注册表撑爆（每多一个条目就是一份熔断状态 + 一个信号量）。
const MAX_SCOPED: usize = 256;

/// 依赖名的长度上限（字节）。
const MAX_DEPENDENCY_NAME: usize = 64;

/// 一套按依赖隔离的治理策略。
#[derive(Debug, Clone)]
struct PolicySet {
    circuit_breaker: CircuitBreaker,
    bulkhead: Bulkhead,
}

/// 出站治理策略集合。
#[derive(Debug, Clone)]
pub struct PolicyRegistry {
    retry: RetryPolicy,
    timeout: Duration,
    config: ResilienceSection,
    /// 默认（共享）策略集，供 [`execute`](Self::execute) 使用。
    default: PolicySet,
    /// 按依赖名隔离的策略集，惰性创建。
    scoped: Arc<Mutex<HashMap<String, PolicySet>>>,
}

impl PolicyRegistry {
    /// 由 `[resilience]` 配置构建。
    #[must_use]
    pub fn from_config(config: &ResilienceSection) -> Self {
        let default = PolicySet {
            circuit_breaker: CircuitBreaker::from_config(config),
            bulkhead: Bulkhead::from_config(config),
        };
        Self {
            retry: RetryPolicy::from_config(config),
            timeout: config.timeout.default,
            config: *config,
            default,
            scoped: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 重试策略。
    #[must_use]
    pub fn retry(&self) -> &RetryPolicy {
        &self.retry
    }

    /// 共享熔断器（`execute` 使用的那一个）。
    #[must_use]
    pub fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.default.circuit_breaker
    }

    /// 共享舱壁（`execute` 使用的那一个）。
    #[must_use]
    pub fn bulkhead(&self) -> &Bulkhead {
        &self.default.bulkhead
    }

    /// 出站默认超时。
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 取（或惰性创建）某个依赖专属的熔断器；用于观测与单测。
    #[must_use]
    pub fn circuit_breaker_for(&self, dependency: &str) -> CircuitBreaker {
        self.policy_set(dependency).circuit_breaker
    }

    /// 取（或惰性创建）某个依赖专属的舱壁；用于观测与单测。
    #[must_use]
    pub fn bulkhead_for(&self, dependency: &str) -> Bulkhead {
        self.policy_set(dependency).bulkhead
    }

    /// 用**共享**策略执行一组出站调用（舱壁 + 熔断 + 重试 + 超时）。
    ///
    /// # Errors
    /// 任一治理环节拒绝或调用失败时返回对应错误。
    pub async fn execute<F, Fut, T>(&self, operation: F) -> AppResult<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = AppResult<T>>,
    {
        self.run(&self.default, operation).await
    }

    /// 用**该依赖专属**的策略执行一组出站调用。
    ///
    /// 同一 `dependency` 的调用共享熔断计数与舱壁名额，不同依赖之间互不影响。
    /// 名字应当是代码里的常量（如 `"payment-gateway"`），不要用请求参数。
    ///
    /// # Errors
    /// 任一治理环节拒绝或调用失败时返回对应错误。
    pub async fn execute_scoped<F, Fut, T>(&self, dependency: &str, operation: F) -> AppResult<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = AppResult<T>>,
    {
        let set = self.policy_set(dependency);
        self.run(&set, operation).await
    }

    /// 治理主循环：每次尝试独立占名额、独立过熔断。
    async fn run<F, Fut, T>(&self, set: &PolicySet, mut operation: F) -> AppResult<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = AppResult<T>>,
    {
        let mut attempt = 1;
        loop {
            // 舱壁：每次尝试独立占名额，退避期间不持有
            let permit = set.bulkhead.try_acquire()?;
            // 熔断：每次尝试都准入，让熔断器看到真实的失败次数
            let probe = set.circuit_breaker.admit().await?;
            // 超时只约束单次尝试
            let result = bulkhead::with_timeout(self.timeout, operation()).await;
            drop(permit);

            match result {
                Ok(value) => {
                    probe.success().await;
                    return Ok(value);
                }
                Err(error) => {
                    probe.failure(&error).await;
                    if !self.retry.should_retry(&error, attempt) {
                        return Err(error);
                    }
                    // 本次失败已经把熔断器打开：再试只会拿到 `50006`，把真实的上游错误
                    // 换成「熔断中」。直接返回原始错误，让调用方看到根因。
                    if set.circuit_breaker.is_open().await {
                        tracing::warn!(
                            attempt,
                            code = %error.code(),
                            "本次失败已触发熔断，停止重试并返回原始错误"
                        );
                        return Err(error);
                    }
                    let delay = self.retry.delay_for(attempt);
                    tracing::warn!(
                        attempt,
                        max_attempts = self.retry.max_attempts(),
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

    /// 取（或惰性创建）某依赖的策略集；名字非法或条目已满时退回共享策略。
    fn policy_set(&self, dependency: &str) -> PolicySet {
        let mut map = self
            .scoped
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(set) = map.get(dependency) {
            return set.clone();
        }
        if dependency.is_empty() || dependency.len() > MAX_DEPENDENCY_NAME {
            tracing::warn!(
                dependency_len = dependency.len(),
                "出站依赖名非法（空或超过 64 字节），回退到共享策略"
            );
            return self.default.clone();
        }
        if map.len() >= MAX_SCOPED {
            tracing::warn!(
                dependency,
                max = MAX_SCOPED,
                "出站依赖名已达上限，回退到共享策略（依赖名应当是常量）"
            );
            return self.default.clone();
        }
        let set = PolicySet {
            circuit_breaker: CircuitBreaker::from_config(&self.config),
            bulkhead: Bulkhead::from_config(&self.config),
        };
        map.insert(dependency.to_string(), set.clone());
        set
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::errors::{AppError, ErrorCode};

    fn config() -> ResilienceSection {
        let mut config = ResilienceSection::default();
        config.retry.max_attempts = 2;
        config.retry.base_delay = Duration::from_millis(1);
        config.retry.max_delay = Duration::from_millis(5);
        config.circuit_breaker.failure_threshold = 2;
        config.bulkhead.max_concurrent = 4;
        config.timeout.default = Duration::from_secs(1);
        config
    }

    #[tokio::test]
    async fn should_expose_policies_from_config() {
        let registry = PolicyRegistry::from_config(&config());
        assert_eq!(registry.retry().max_attempts(), 2);
        assert_eq!(registry.bulkhead().max_concurrent(), 4);
        assert_eq!(registry.timeout(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn should_execute_with_retry() {
        let registry = PolicyRegistry::from_config(&config());
        let calls = AtomicU32::new(0);

        let value = registry
            .execute(|| async {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(AppError::internal(anyhow::anyhow!("transient")))
                } else {
                    Ok(11)
                }
            })
            .await
            .unwrap();

        assert_eq!(value, 11);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn should_open_circuit_after_repeated_failures() {
        let registry = PolicyRegistry::from_config(&config());

        for _ in 0..2 {
            let _ = registry
                .execute(|| async { Err::<i32, _>(AppError::internal(anyhow::anyhow!("down"))) })
                .await;
        }

        let error = registry
            .execute(|| async { Ok::<i32, AppError>(1) })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::CIRCUIT_OPEN,
            "连续失败后应快速失败"
        );
    }

    #[tokio::test]
    async fn should_count_every_attempt_in_circuit_breaker() {
        let mut config = config();
        config.retry.max_attempts = 3;
        config.circuit_breaker.failure_threshold = 3;
        let registry = PolicyRegistry::from_config(&config);

        // 单次请求 3 次尝试全部失败 → 阈值 3 被填满 → 熔断打开
        let _ = registry
            .execute(|| async { Err::<i32, _>(AppError::internal(anyhow::anyhow!("down"))) })
            .await;
        assert!(
            registry.circuit_breaker().is_open().await,
            "熔断应按每次尝试计数，而不是每个请求计数"
        );
    }

    #[tokio::test]
    async fn should_not_hold_bulkhead_permit_during_backoff() {
        let mut config = config();
        config.bulkhead.max_concurrent = 1;
        config.retry.max_attempts = 2;
        config.retry.base_delay = Duration::from_millis(150);
        config.retry.max_delay = Duration::from_millis(150);
        let registry = PolicyRegistry::from_config(&config);
        let bulkhead = registry.bulkhead().clone();

        let runner = registry.clone();
        let handle = tokio::spawn(async move {
            runner
                .execute(|| async {
                    Err::<i32, _>(AppError::biz(ErrorCode::UPSTREAM_ERROR, "down"))
                })
                .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            bulkhead.available(),
            1,
            "退避期间必须归还名额，否则一个慢依赖就能占满全部出站名额"
        );
        let _ = handle.await;
    }

    #[tokio::test]
    async fn should_isolate_policies_per_dependency() {
        let mut config = config();
        config.retry.max_attempts = 1;
        config.circuit_breaker.failure_threshold = 1;
        let registry = PolicyRegistry::from_config(&config);

        let _ = registry
            .execute_scoped("payment", || async {
                Err::<i32, _>(AppError::internal(anyhow::anyhow!("down")))
            })
            .await;

        assert!(
            registry.circuit_breaker_for("payment").is_open().await,
            "故障依赖应被熔断"
        );
        assert!(
            !registry.circuit_breaker_for("user-center").is_open().await,
            "其他依赖不得被连带熔断"
        );
        let value = registry
            .execute_scoped("user-center", || async { Ok::<_, AppError>(1) })
            .await
            .unwrap();
        assert_eq!(value, 1);
    }

    #[tokio::test]
    async fn should_fall_back_to_shared_policies_when_name_is_invalid() {
        let registry = PolicyRegistry::from_config(&config());
        let value = registry
            .execute_scoped("", || async { Ok::<_, AppError>(2) })
            .await
            .unwrap();
        assert_eq!(value, 2);
        assert_eq!(
            registry.bulkhead_for("").max_concurrent(),
            registry.bulkhead().max_concurrent(),
            "非法依赖名应退回共享策略"
        );
    }

    #[tokio::test]
    async fn should_return_original_error_when_failure_opens_circuit() {
        let mut config = config();
        config.retry.max_attempts = 3;
        config.circuit_breaker.failure_threshold = 1;
        let registry = PolicyRegistry::from_config(&config);

        let error = registry
            .execute(|| async {
                Err::<i32, _>(AppError::biz(ErrorCode::UPSTREAM_ERROR, "upstream down"))
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::UPSTREAM_ERROR,
            "熔断被本次失败打开时应返回真实错误，而不是把自己触发的 50006 抛给调用方"
        );
    }

    #[tokio::test]
    async fn should_not_retry_when_non_idempotent_and_timed_out() {
        let mut config = config();
        config.retry.max_attempts = 3;
        let registry = PolicyRegistry::from_config(&config);
        let calls = AtomicU32::new(0);

        let error = registry
            .execute(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(AppError::biz(ErrorCode::GATEWAY_TIMEOUT, "slow"))
            })
            .await
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::GATEWAY_TIMEOUT);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "非幂等调用不重试超时");
    }
}
