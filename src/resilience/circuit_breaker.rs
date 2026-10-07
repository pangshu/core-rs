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

use std::sync::Mutex; // 引入标准库 Mutex，保护熔断状态
use std::time::Instant; // 引入 Instant，记录打开/半开时间点

use tokio::sync::Semaphore; // 引入信号量，限制半开探测名额

use crate::config::sections::ResiliencePolicy; // 引入弹性策略（阈值/时长等）
use crate::resilience::ResilienceError; // 引入弹性错误类型

#[derive(Debug, Clone, Copy, PartialEq, Eq)] // 派生常用 trait，状态可比较与复制
enum State { // 熔断器状态机
    Closed, // 关闭：正常放行
    Open { since: Instant }, // 打开：拒绝请求，since 为打开时刻
    HalfOpen { since: Instant }, // 半开：放行少量探测，since 为半开起点
}

/// 状态 + 计数 + 时间戳：单一同步原语，杜绝跨原语交错
#[derive(Debug, Clone, Copy)] // 可复制，便于持锁期间读写
struct CbState { // 熔断器内部共享状态
    state: State, // 当前状态
    failures: u32, // 连续失败计数
}

pub struct CircuitBreaker { // 熔断器：按依赖维度隔离
    dep: String, // 依赖名（日志与错误使用）
    policy: ResiliencePolicy, // 该依赖的策略
    inner: Mutex<CbState>, // 保护状态与计数的互斥锁
    /// 半开探测名额
    half_open_permits: std::sync::Arc<Semaphore>, // 半开期放行的探测请求名额
}

impl CircuitBreaker {
    pub fn new(dep: impl Into<String>, policy: ResiliencePolicy) -> Self { // 按依赖名与策略创建熔断器
        let permits = policy.half_open_max_calls.max(1); // 半开探测名额至少 1
        Self {
            dep: dep.into(), // 转为 String 保存依赖名
            policy, // 保存策略
            inner: Mutex::new(CbState { // 初始化内部状态
                state: State::Closed, // 初始为关闭态
                failures: 0, // 失败计数归零
            }),
            half_open_permits: std::sync::Arc::new(Semaphore::new(permits as usize)), // 初始化半开名额信号量
        }
    }

    pub fn dep(&self) -> &str { // 返回依赖名
        &self.dep // 借用字段
    }

    /// 就地推进状态机（持锁调用）：Open 超时 → HalfOpen（起点取 Open 的
    /// 过期时刻，而非本次调用时刻——长时间无人调用时看门狗同样生效）；
    /// HalfOpen 停留超时（探测全挂死）→ 判失败重新 Open
    fn advance(lock: &mut CbState, policy: &ResiliencePolicy) { // 依据时间推进状态机
        let open_secs = std::time::Duration::from_secs(policy.open_secs.max(1)); // 打开/半开窗口时长（至少 1s）
        match lock.state { // 按当前状态分支
            State::Open { since } => { // 打开态
                let expired_at = since + open_secs; // 计算打开窗口过期时刻
                if Instant::now() >= expired_at { // 已过期
                    lock.state = State::HalfOpen { since: expired_at }; // 转入半开，起点取过期时刻
                }
            }
            State::HalfOpen { since } => { // 半开态
                if since.elapsed() >= open_secs { // 半开停留超时（探测全挂死）
                    // 看门狗：半开探测窗口耗尽仍未恢复，重新打开（再等一个周期）
                    lock.state = State::Open { // 重新打开
                        since: Instant::now(), // 记录新的打开时刻
                    };
                }
            }
            State::Closed => {} // 关闭态无需处理
        }
    }

    /// 包裹一次对依赖的调用
    pub async fn call<T, E, Fut>(&self, fut: Fut) -> Result<T, ResilienceError<E>> // 经熔断器执行一次调用
    where
        Fut: std::future::Future<Output = Result<T, E>>, // 待执行的业务 Future
        E: std::fmt::Display, // 错误需可打印
    {
        // 状态检查与半开名额获取（permit 在本函数结束时自然归还）
        let permit = { // 在受限作用域内持锁做状态判定
            let mut lock = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner); // 获取锁，poison 时取回内部值
            Self::advance(&mut lock, &self.policy); // 先推进状态机
            match lock.state { // 按推进后的状态决定放行
                State::Closed => None, // 关闭态：无需名额
                State::Open { .. } => { // 打开态：直接拒绝
                    return Err(ResilienceError::Open { dep: self.dep.clone() }); // 返回熔断拒绝错误
                }
                State::HalfOpen { .. } => match self.half_open_permits.clone().try_acquire_owned() { // 半开态：尝试取探测名额
                    Ok(p) => Some(p), // 有名额：放行探测
                    Err(_) => { // 无名额：名额已耗尽
                        return Err(ResilienceError::Open { dep: self.dep.clone() }); // 视为拒绝
                    }
                },
            }
        };
        let _permit = permit; // 持有名额直到函数结束（结束即归还）

        match fut.await { // 执行真正的调用
            Ok(value) => { // 成功
                self.on_success(); // 记录成功（可能关闭熔断）
                Ok(value) // 返回成功值
            }
            Err(e) => { // 失败
                self.on_failure(&e); // 记录失败（可能打开熔断）
                Err(ResilienceError::Exhausted { attempts: 1, source: e }) // 包装为单次耗尽错误
            }
        }
    }

    fn on_success(&self) { // 记录一次成功
        let mut lock = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner); // 获取锁，poison 时取回内部值
        Self::advance(&mut lock, &self.policy); // 先推进状态机
        match lock.state { // 按状态决定成功的影响
            // Open 态下返回的"成功"是在熔断决策之前放行的在途请求，
            // 属陈旧结果——不得擦除刚发生的熔断决策
            State::Open { .. } => {} // 打开态：忽略该陈旧成功
            State::HalfOpen { .. } => { // 半开态：探测成功即恢复
                tracing::info!(dep = %self.dep, "circuit closed (recovered)"); // 记录恢复日志
                lock.state = State::Closed; // 转回关闭态
                lock.failures = 0; // 失败计数归零
            }
            State::Closed => { // 关闭态
                lock.failures = 0; // 成功即清零连续失败计数
            }
        }
    }

    fn on_failure<E: std::fmt::Display>(&self, e: &E) { // 记录一次失败
        let mut lock = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner); // 获取锁，poison 时取回内部值
        Self::advance(&mut lock, &self.policy); // 先推进状态机
        lock.failures = lock.failures.saturating_add(1); // 失败计数饱和加一
        match lock.state { // 按状态决定失败的影响
            State::Closed => { // 关闭态
                if lock.failures >= self.policy.failure_threshold.max(1) { // 达到阈值
                    tracing::warn!(dep = %self.dep, failures = lock.failures, "circuit opened"); // 记录熔断打开
                    lock.state = State::Open { // 转入打开态
                        since: Instant::now(), // 记录打开时刻
                    };
                }
            }
            State::HalfOpen { .. } => { // 半开态：探测失败
                tracing::warn!(dep = %self.dep, error = %e, "half-open probe failed, circuit re-opened"); // 记录探测失败
                lock.state = State::Open { // 重新打开
                    since: Instant::now(), // 记录打开时刻
                };
            }
            State::Open { .. } => {} // 打开态：无需额外处理
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
