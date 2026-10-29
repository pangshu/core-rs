//! 熔断器（`[resilience].circuit_breaker`）。
//!
//! 状态机：`Closed`（放行）→ 连续失败达阈值 → `Open`（快速失败 `50006`）→
//! 经过 `open_duration` → `HalfOpen`（放行少量探测）→ 探测成功 → `Closed`。
//!
//! 作用于**出站调用**，不参与入站中间件链（见 `docs/02` §11）。
//!
//! # 半开探测名额为什么由 `Drop` 归还
//!
//! `HalfOpen` 下放行的探测数受 `half_open_requests` 约束。名额必须与**请求生命周期**
//! 绑定，而不是与「调用方是否走到记账那一步」绑定：请求被取消（入站 `408`、
//! 客户端断连、上层 `select!` 丢弃 future）或 operation panic 时，记账代码根本
//! 不会执行。若名额只由显式调用归还，漏掉一次就再也放不出探测——`HalfOpen` 会
//! 永久卡死，该依赖此后全部快速失败。因此名额用独立的原子计数，由
//! [`ProbeGuard`] 的 `Drop` 兜底归还（`Drop` 不能 `await`，所以不能用那把
//! `tokio::sync::Mutex`）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::ResilienceSection;
use crate::errors::{AppError, AppResult, ErrorCode};

/// 熔断状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// 正常放行。
    Closed,
    /// 熔断中，快速失败。
    Open,
    /// 半开，放行少量探测请求。
    HalfOpen,
}

/// 熔断器配置。
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    /// 连续失败阈值。
    pub failure_threshold: u32,
    /// 熔断持续时长。
    pub open_duration: Duration,
    /// 半开状态下允许的并发探测数。
    pub half_open_requests: u32,
}

impl CircuitBreakerConfig {
    /// 由 `[resilience]` 配置构造。
    #[must_use]
    pub fn from_config(config: &ResilienceSection) -> Self {
        Self {
            failure_threshold: config.circuit_breaker.failure_threshold.max(1),
            open_duration: config.circuit_breaker.open_duration,
            half_open_requests: config.circuit_breaker.half_open_requests.max(1),
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    state: Option<CircuitState>,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    half_open_successes: u32,
}

impl Inner {
    fn state(&self) -> CircuitState {
        self.state.unwrap_or(CircuitState::Closed)
    }
}

/// 熔断器。
///
/// 以 `Arc` 共享；同一实例的所有出站调用共享计数。
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    inner: Arc<Mutex<Inner>>,
    /// `HalfOpen` 下已放行的探测数（独立于 `inner`，`Drop` 里可直接原子归还）。
    half_open_inflight: Arc<AtomicU32>,
    /// 半开轮次：每次 `Open → HalfOpen` 递增，用于丢弃上一轮迟到的归还。
    epoch: Arc<AtomicU64>,
}

/// 一次放行凭据。
///
/// [`CircuitBreaker::admit`] / [`CircuitBreaker::call`] 返回。**必须**持有到调用
/// 结束：`Drop` 时归还 `HalfOpen` 探测名额，所以「取消 / panic / 忘记记账」都不会
/// 泄漏名额。正常路径用 [`success`](Self::success) / [`failure`](Self::failure)
/// 记账，二者会先解除 `Drop` 的兜底归还再更新状态。
#[derive(Debug)]
#[must_use = "探测名额在 guard 释放时归还；立刻丢弃等于放弃这次记账"]
pub struct ProbeGuard {
    breaker: CircuitBreaker,
    epoch: u64,
    armed: bool,
}

impl ProbeGuard {
    /// 一次 `Closed` 状态下的普通放行（不占探测名额）。
    fn pass(breaker: &CircuitBreaker) -> Self {
        Self {
            breaker: breaker.clone(),
            epoch: 0,
            armed: false,
        }
    }

    /// 占用一个半开探测名额。
    fn probe(breaker: &CircuitBreaker) -> Self {
        Self {
            breaker: breaker.clone(),
            epoch: breaker.epoch.load(Ordering::Acquire),
            armed: true,
        }
    }

    /// 记录成功并结束本次放行。
    pub async fn success(mut self) {
        self.release();
        self.breaker.record_success().await;
    }

    /// 记录失败并结束本次放行；仅 5xx 类计入熔断。
    pub async fn failure(mut self, error: &AppError) {
        self.release();
        self.breaker.on_error(error).await;
    }

    /// 归还探测名额（幂等：`armed` 置位后不会重复归还）。
    fn release(&mut self) {
        if self.armed {
            self.armed = false;
            self.breaker.release_probe(self.epoch);
        }
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl CircuitBreaker {
    /// 按配置创建（初始 `Closed`）。
    #[must_use]
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            inner: Arc::new(Mutex::new(Inner::default())),
            half_open_inflight: Arc::new(AtomicU32::new(0)),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 按 `[resilience]` 配置创建。
    #[must_use]
    pub fn from_config(config: &ResilienceSection) -> Self {
        Self::new(CircuitBreakerConfig::from_config(config))
    }

    /// 当前状态（会自动把「已到期的 `Open`」推进为 `HalfOpen`）。
    pub async fn state(&self) -> CircuitState {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);
        inner.state()
    }

    /// 是否处于熔断（`Open`）。
    pub async fn is_open(&self) -> bool {
        self.state().await == CircuitState::Open
    }

    /// 执行一次出站调用，并按结果记账。
    ///
    /// # Errors
    /// - 熔断打开 / 半开探测名额已满 → `50006`（`CIRCUIT_OPEN`）
    /// - 调用本身失败时原样返回
    pub async fn call<F, Fut, T>(&self, operation: F) -> AppResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = AppResult<T>>,
    {
        let probe = self.admit().await?;

        match operation().await {
            Ok(value) => {
                probe.success().await;
                Ok(value)
            }
            Err(error) => {
                probe.failure(&error).await;
                Err(error)
            }
        }
    }

    /// 准入判定，返回必须持有到调用结束的 [`ProbeGuard`]。
    ///
    /// 供 [`PolicyRegistry`](crate::resilience::PolicyRegistry) 组合使用：**每次
    /// 尝试都要重新准入**，这样熔断器看到的是「每次尝试」而不是「整个重试序列」。
    ///
    /// # Errors
    /// 熔断打开或半开名额已满时返回 `50006`。
    pub async fn admit(&self) -> AppResult<ProbeGuard> {
        self.acquire().await
    }

    /// 记录一次成功。
    pub async fn record_success(&self) {
        let mut inner = self.inner.lock().await;
        match inner.state() {
            CircuitState::HalfOpen => {
                inner.half_open_successes += 1;
                if inner.half_open_successes >= self.config.half_open_requests {
                    tracing::info!("熔断器恢复：HalfOpen → Closed");
                    inner.state = Some(CircuitState::Closed);
                    inner.consecutive_failures = 0;
                    inner.opened_at = None;
                    inner.half_open_successes = 0;
                }
            }
            _ => inner.consecutive_failures = 0,
        }
    }

    /// 记录一次失败。
    pub async fn record_failure(&self) {
        let mut inner = self.inner.lock().await;
        match inner.state() {
            CircuitState::HalfOpen => {
                inner.half_open_successes = 0;
                tracing::warn!("熔断器：HalfOpen 探测失败 → Open");
                inner.state = Some(CircuitState::Open);
                inner.opened_at = Some(Instant::now());
            }
            _ => {
                inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                if inner.consecutive_failures >= self.config.failure_threshold {
                    tracing::warn!(
                        failures = inner.consecutive_failures,
                        "熔断器打开：Closed → Open"
                    );
                    inner.state = Some(CircuitState::Open);
                    inner.opened_at = Some(Instant::now());
                }
            }
        }
    }

    /// 一次失败的记账入口：只有「值得重试」的码计入熔断，其余仅归还探测名额。
    async fn on_error(&self, error: &AppError) {
        if is_retryable(error.code()) {
            self.record_failure().await;
        }
    }

    /// 放行判定：`Closed` 直接放行；`Open` 到期转 `HalfOpen`；`HalfOpen` 限流探测。
    async fn acquire(&self) -> AppResult<ProbeGuard> {
        let mut inner = self.inner.lock().await;
        self.refresh(&mut inner);

        match inner.state() {
            CircuitState::Closed => Ok(ProbeGuard::pass(self)),
            CircuitState::HalfOpen => self.take_probe_slot().map(|()| ProbeGuard::probe(self)),
            CircuitState::Open => Err(rejected("下游熔断中，请稍后重试")),
        }
    }

    /// 尝试占一个半开探测名额（CAS 循环，无锁）。
    fn take_probe_slot(&self) -> AppResult<()> {
        let mut current = self.half_open_inflight.load(Ordering::Acquire);
        loop {
            if current >= self.config.half_open_requests {
                return Err(rejected("熔断器半开探测名额已满"));
            }
            match self.half_open_inflight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    /// 归还探测名额；`epoch` 不匹配说明这是上一轮的迟到归还，直接丢弃。
    fn release_probe(&self, epoch: u64) {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        let mut current = self.half_open_inflight.load(Ordering::Acquire);
        loop {
            let next = current.saturating_sub(1);
            match self.half_open_inflight.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// 把到期的 `Open` 推进为 `HalfOpen`。
    fn refresh(&self, inner: &mut Inner) {
        if inner.state() != CircuitState::Open {
            return;
        }
        let elapsed = inner.opened_at.map(|at| at.elapsed()).unwrap_or_default();
        if elapsed >= self.config.open_duration {
            tracing::info!("熔断器进入 HalfOpen，放行探测请求");
            inner.state = Some(CircuitState::HalfOpen);
            inner.half_open_successes = 0;
            // 新一轮探测：递增 epoch 后清零计数，上一轮未归还的探测随之作废
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.half_open_inflight.store(0, Ordering::Release);
        }
    }
}

/// 是否值得计入熔断 / 值得重试：仅 5xx 类**依赖故障**。
///
/// **不含** [`ErrorCode::SERVICE_UNAVAILABLE`]：那是本进程的「未就绪」语义，
/// 重试它只会把启动/停机窗口拉长。
#[must_use]
pub fn is_retryable(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::INTERNAL
            | ErrorCode::DB_ERROR
            | ErrorCode::UPSTREAM_ERROR
            | ErrorCode::CACHE_ERROR
            | ErrorCode::GATEWAY_TIMEOUT
    )
}

/// 是否为**本进程自身**的拒绝（熔断打开 / 舱壁已满）。
///
/// 这类码不能当「下游故障」处理：否则服务 A 的自我保护会被服务 B 记成失败，
/// 一路重试并级联熔断——每个实例都在替别人打自己。
#[must_use]
pub fn is_self_rejection(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::CIRCUIT_OPEN | ErrorCode::BULKHEAD_FULL)
}

/// 是否允许降级（[`Fallback`](crate::resilience::Fallback) 的唯一判定依据）。
///
/// 只覆盖「依赖不可用」这一类：依赖故障 + 自身拒绝 + 本进程未就绪。
/// 认证 / 授权 / 限流 / 参数 / 业务错误**一律不允许降级**——把 401 兜底成默认值
/// 就是 fail-open。
#[must_use]
pub fn is_degradable(code: ErrorCode) -> bool {
    is_retryable(code) || is_self_rejection(code) || code == ErrorCode::SERVICE_UNAVAILABLE
}

fn rejected(message: &str) -> AppError {
    AppError::biz(ErrorCode::CIRCUIT_OPEN, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 2,
            open_duration: Duration::from_millis(50),
            half_open_requests: 1,
        }
    }

    #[tokio::test]
    async fn should_start_closed() {
        let breaker = CircuitBreaker::new(config());
        assert_eq!(breaker.state().await, CircuitState::Closed);
        assert!(!breaker.is_open().await);
    }

    #[tokio::test]
    async fn should_open_after_threshold_failures() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        assert_eq!(breaker.state().await, CircuitState::Closed, "未达阈值");

        breaker.record_failure().await;
        assert_eq!(breaker.state().await, CircuitState::Open);
    }

    #[tokio::test]
    async fn should_fail_fast_when_open() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        breaker.record_failure().await;

        let error = breaker
            .call(|| async { Ok::<_, AppError>(1) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::CIRCUIT_OPEN);
        assert_eq!(error.code().status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn should_half_open_after_open_duration_and_close_on_success() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        breaker.record_failure().await;

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(breaker.state().await, CircuitState::HalfOpen);

        breaker
            .call(|| async { Ok::<_, AppError>(42) })
            .await
            .unwrap();
        assert_eq!(
            breaker.state().await,
            CircuitState::Closed,
            "探测成功应闭合"
        );
    }

    #[tokio::test]
    async fn should_reopen_when_half_open_probe_fails() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        breaker.record_failure().await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        let _ = breaker
            .call(|| async { Err::<i32, _>(AppError::internal(anyhow::anyhow!("boom"))) })
            .await;
        assert_eq!(breaker.state().await, CircuitState::Open);
    }

    #[tokio::test]
    async fn should_not_count_business_errors_as_failures() {
        let breaker = CircuitBreaker::new(config());
        for _ in 0..5 {
            let _ = breaker
                .call(|| async { Err::<i32, _>(AppError::not_found("user")) })
                .await;
        }
        assert_eq!(
            breaker.state().await,
            CircuitState::Closed,
            "4xx 业务错误不应触发熔断"
        );
    }

    #[tokio::test]
    async fn should_reset_failures_on_success() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        breaker.record_success().await;
        breaker.record_failure().await;
        assert_eq!(
            breaker.state().await,
            CircuitState::Closed,
            "成功应清零计数"
        );
    }

    #[tokio::test]
    async fn should_release_probe_slot_when_call_is_cancelled() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        breaker.record_failure().await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(breaker.state().await, CircuitState::HalfOpen);

        // 占住唯一的半开探测名额，然后取消（等价于入站 408 / 客户端断连）
        let probing = breaker.clone();
        let handle = tokio::spawn(async move {
            probing
                .call(|| async { std::future::pending::<AppResult<()>>().await })
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        let busy = breaker
            .call(|| async { Ok::<_, AppError>(()) })
            .await
            .unwrap_err();
        assert_eq!(busy.code(), ErrorCode::CIRCUIT_OPEN, "名额被占时应快速失败");

        handle.abort();
        let _ = handle.await;

        // 关键回归：取消后名额必须已归还，否则熔断器永久卡在 HalfOpen
        let value = breaker
            .call(|| async { Ok::<_, AppError>(7) })
            .await
            .unwrap();
        assert_eq!(value, 7, "取消的探测不应泄漏名额");
        assert_eq!(breaker.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn should_discard_late_release_from_previous_epoch() {
        let breaker = CircuitBreaker::new(config());
        breaker.record_failure().await;
        breaker.record_failure().await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        // 第一轮探测（epoch 1）占住名额后卡住
        let stale = breaker.admit().await.unwrap();
        breaker.record_failure().await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        // 重新进入 HalfOpen（epoch 2，名额清零），新探测占住唯一名额
        assert_eq!(breaker.state().await, CircuitState::HalfOpen);
        let fresh = breaker.admit().await.unwrap();

        // 上一轮迟到的归还不得把新一轮的名额扣掉
        drop(stale);
        assert!(
            breaker.admit().await.is_err(),
            "旧轮次的归还不能放行超额探测"
        );
        drop(fresh);
    }

    #[test]
    fn should_classify_retryable_codes() {
        assert!(is_retryable(ErrorCode::INTERNAL));
        assert!(is_retryable(ErrorCode::UPSTREAM_ERROR));
        assert!(is_retryable(ErrorCode::GATEWAY_TIMEOUT));
        assert!(!is_retryable(ErrorCode::NOT_FOUND));
        assert!(!is_retryable(ErrorCode::INVALID_PARAM));
        assert!(
            !is_retryable(ErrorCode::SERVICE_UNAVAILABLE),
            "未就绪不是依赖故障"
        );
        assert!(
            !is_retryable(ErrorCode::CIRCUIT_OPEN),
            "自身拒绝不能重试，否则跨服务重试风暴"
        );
        assert!(!is_retryable(ErrorCode::BULKHEAD_FULL));
    }

    #[test]
    fn should_only_degrade_dependency_failures() {
        assert!(is_degradable(ErrorCode::UPSTREAM_ERROR));
        assert!(is_degradable(ErrorCode::CIRCUIT_OPEN));
        assert!(is_degradable(ErrorCode::BULKHEAD_FULL));
        assert!(is_degradable(ErrorCode::SERVICE_UNAVAILABLE));
        // 安全相关错误一律不得降级（fail-open）
        assert!(!is_degradable(ErrorCode::UNAUTHENTICATED));
        assert!(!is_degradable(ErrorCode::FORBIDDEN));
        assert!(!is_degradable(ErrorCode::PERMISSION_DENIED));
        assert!(!is_degradable(ErrorCode::RATE_LIMITED));
        assert!(!is_degradable(ErrorCode::INVALID_PARAM));
        assert!(!is_degradable(ErrorCode::NOT_FOUND));
    }
}
