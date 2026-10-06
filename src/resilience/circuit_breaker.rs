//! 熔断器：连续失败 → 打开 → 半开探测（文档 三·7）。
//!
//! - Closed：正常放行；连续失败达 `failure_threshold` → Open；
//! - Open：直接拒绝（[`ResilienceError::Open`]），持续 `open_secs`；
//! - HalfOpen：放行至多 `half_open_max_calls` 个探测请求，成功则关闭，
//!   探测失败重新打开；半开停留超过 `open_secs` 仍未恢复 → 判探测失败
//!   重新 Open（看门狗：探测请求全部挂死时状态机不能停在 HalfOpen）。
//!
//! 状态、失败计数、半开起始时间收敛在**同一把 Mutex** 里：拆成独立原子量时，
//! 一个在途成功请求会在熔断刚打开后把状态擦回 Closed（经典的"偶发成功击穿"）。

use std::sync::Mutex;
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::config::sections::ResiliencePolicy;
use crate::resilience::ResilienceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open { since: Instant },
    HalfOpen { since: Instant },
}

/// 状态 + 计数 + 时间戳：单一同步原语，杜绝跨原语交错
#[derive(Debug, Clone, Copy)]
struct CbState {
    state: State,
    failures: u32,
}

pub struct CircuitBreaker {
    dep: String,
    policy: ResiliencePolicy,
    inner: Mutex<CbState>,
    /// 半开探测名额
    half_open_permits: std::sync::Arc<Semaphore>,
}

impl CircuitBreaker {
    pub fn new(dep: impl Into<String>, policy: ResiliencePolicy) -> Self {
        let permits = policy.half_open_max_calls.max(1);
        Self {
            dep: dep.into(),
            policy,
            inner: Mutex::new(CbState {
                state: State::Closed,
                failures: 0,
            }),
            half_open_permits: std::sync::Arc::new(Semaphore::new(permits as usize)),
        }
    }

    pub fn dep(&self) -> &str {
        &self.dep
    }

    /// 就地推进状态机（持锁调用）：Open 超时 → HalfOpen（起点取 Open 的
    /// 过期时刻，而非本次调用时刻——长时间无人调用时看门狗同样生效）；
    /// HalfOpen 停留超时（探测全挂死）→ 判失败重新 Open
    fn advance(lock: &mut CbState, policy: &ResiliencePolicy) {
        let open_secs = std::time::Duration::from_secs(policy.open_secs.max(1));
        match lock.state {
            State::Open { since } => {
                let expired_at = since + open_secs;
                if Instant::now() >= expired_at {
                    lock.state = State::HalfOpen { since: expired_at };
                }
            }
            State::HalfOpen { since } => {
                if since.elapsed() >= open_secs {
                    // 看门狗：半开探测窗口耗尽仍未恢复，重新打开（再等一个周期）
                    lock.state = State::Open {
                        since: Instant::now(),
                    };
                }
            }
            State::Closed => {}
        }
    }

    /// 包裹一次对依赖的调用
    pub async fn call<T, E, Fut>(&self, fut: Fut) -> Result<T, ResilienceError<E>>
    where
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        // 状态检查与半开名额获取（permit 在本函数结束时自然归还）
        let permit = {
            let mut lock = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::advance(&mut lock, &self.policy);
            match lock.state {
                State::Closed => None,
                State::Open { .. } => {
                    return Err(ResilienceError::Open { dep: self.dep.clone() });
                }
                State::HalfOpen { .. } => match self.half_open_permits.clone().try_acquire_owned() {
                    Ok(p) => Some(p),
                    Err(_) => {
                        return Err(ResilienceError::Open { dep: self.dep.clone() });
                    }
                },
            }
        };
        let _permit = permit;

        match fut.await {
            Ok(value) => {
                self.on_success();
                Ok(value)
            }
            Err(e) => {
                self.on_failure(&e);
                Err(ResilienceError::Exhausted { attempts: 1, source: e })
            }
        }
    }

    fn on_success(&self) {
        let mut lock = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::advance(&mut lock, &self.policy);
        match lock.state {
            // Open 态下返回的"成功"是在熔断决策之前放行的在途请求，
            // 属陈旧结果——不得擦除刚发生的熔断决策
            State::Open { .. } => {}
            State::HalfOpen { .. } => {
                tracing::info!(dep = %self.dep, "circuit closed (recovered)");
                lock.state = State::Closed;
                lock.failures = 0;
            }
            State::Closed => {
                lock.failures = 0;
            }
        }
    }

    fn on_failure<E: std::fmt::Display>(&self, e: &E) {
        let mut lock = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::advance(&mut lock, &self.policy);
        lock.failures = lock.failures.saturating_add(1);
        match lock.state {
            State::Closed => {
                if lock.failures >= self.policy.failure_threshold.max(1) {
                    tracing::warn!(dep = %self.dep, failures = lock.failures, "circuit opened");
                    lock.state = State::Open {
                        since: Instant::now(),
                    };
                }
            }
            State::HalfOpen { .. } => {
                tracing::warn!(dep = %self.dep, error = %e, "half-open probe failed, circuit re-opened");
                lock.state = State::Open {
                    since: Instant::now(),
                };
            }
            State::Open { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sections::ResiliencePolicy;
    use std::time::Duration;

    fn policy(open_secs: u64) -> ResiliencePolicy {
        ResiliencePolicy {
            failure_threshold: 2,
            open_secs,
            ..Default::default()
        }
    }

    async fn fail<T>(breaker: &CircuitBreaker) where T: Default {
        let _: Result<T, _> = breaker
            .call(async { Err::<T, &str>("boom") })
            .await;
    }

    /// Closed → Open → HalfOpen → Closed 全流程（open_secs=1 可实测驱动）
    #[tokio::test]
    async fn open_halfopen_closed_cycle() {
        let breaker = CircuitBreaker::new("dep", policy(1));

        fail::<i32>(&breaker).await;
        fail::<i32>(&breaker).await;

        // 阈值达到 → Open：直接拒绝
        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(res.unwrap_err().is_open(), "达到阈值后必须 Open");

        // open_secs 过后 → HalfOpen：探测成功 → Closed
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(res.is_ok(), "半开探测应放行并恢复 Closed");

        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(res.is_ok(), "恢复 Closed 后正常放行");
    }

    /// 半开探测失败 → 重新 Open；Open 态下返回的成功不得擦除熔断决策
    #[tokio::test]
    async fn halfopen_failure_reopens_and_stale_success_is_ignored() {
        let breaker = CircuitBreaker::new("dep", policy(1));

        fail::<i32>(&breaker).await;
        fail::<i32>(&breaker).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // 半开探测失败 → 重新 Open
        let res: Result<i32, _> = breaker.call(async { Err::<i32, &str>("still down") }).await;
        assert!(matches!(res, Err(ResilienceError::Exhausted { .. })));
        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(res.unwrap_err().is_open(), "探测失败后必须重新 Open");

        // Open 期间收到一个"成功"（陈旧在途请求）：不得变为 Closed
        //（通过内部锁直接模拟 on_success 的陈旧路径）
        breaker.on_success();
        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(
            res.unwrap_err().is_open(),
            "陈旧成功不得擦除熔断决策（P1-34）"
        );
    }

    /// 半开看门狗：探测全部挂死时，超过窗口后状态机不得停留在 HalfOpen
    #[tokio::test]
    async fn halfopen_watchdog_reopens_when_probes_hang() {
        let breaker = CircuitBreaker::new("dep", policy(1));
        fail::<i32>(&breaker).await;
        fail::<i32>(&breaker).await;
        // 模拟挂死的探测：占住半开名额且从不归还/完成
        let _permit = breaker
            .half_open_permits
            .clone()
            .try_acquire_owned()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(1100)).await;
        // 进入 HalfOpen，但名额被挂死探测占满 → 新请求被拒
        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(res.unwrap_err().is_open(), "半开名额耗尽应拒绝新请求");

        // 看门狗：探测窗口耗尽仍未恢复 → 重新 Open（而非停留在 HalfOpen）
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let res: Result<i32, _> = breaker.call(async { Ok::<i32, &str>(1) }).await;
        assert!(
            res.unwrap_err().is_open(),
            "半开探测窗口耗尽必须重新 Open（P1-35）"
        );
    }
}
