//! 弹性：熔断 / 重试 / 降级 / 舱壁（文档 三·7）。外部依赖（第三方 API、DB、缓存）
//! 抖动时按策略隔离，避免级联故障。纯 tokio 实现，无外部依赖，常开。
//!
//! 策略配置集中在 `[resilience]`（各依赖的阈值 / 重试次数 / 降级开关）：
//!
//! ```toml
//! [resilience.default]
//! failure_threshold = 5
//! open_secs = 30
//! max_retries = 2
//!
//! [resilience.deps."payment-api"]
//! failure_threshold = 3
//! fallback_enabled = true
//! ```

pub mod bulkhead;
pub mod circuit_breaker;
pub mod fallback;
pub mod retry;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::config::sections::{ResiliencePolicy, ResilienceSettings};

/// 按依赖名持有各自的熔断器与舱壁（进程内共享；App 装配时创建，存于应用侧）
#[derive(Clone, Default)]
pub struct Registry {
    settings: Arc<RwLock<ResilienceSettings>>,
    breakers: Arc<RwLock<HashMap<String, Arc<circuit_breaker::CircuitBreaker>>>>,
    bulkheads: Arc<RwLock<HashMap<String, Arc<bulkhead::Bulkhead>>>>,
}

impl Registry {
    pub fn new(settings: ResilienceSettings) -> Self {
        Self {
            settings: Arc::new(RwLock::new(settings)),
            breakers: Default::default(),
            bulkheads: Default::default(),
        }
    }

    /// 更新策略（可挂到 config watcher 订阅实现热更新）
    pub async fn update_settings(&self, settings: ResilienceSettings) {
        *self.settings.write().await = settings;
    }

    /// 取依赖的当前策略
    pub async fn policy(&self, dep: &str) -> ResiliencePolicy {
        self.settings.read().await.policy(dep)
    }

    /// 取（或创建）依赖的熔断器
    pub async fn breaker(&self, dep: &str) -> Arc<circuit_breaker::CircuitBreaker> {
        let policy = self.policy(dep).await;
        let mut map = self.breakers.write().await;
        map.entry(dep.to_string())
            .or_insert_with(|| Arc::new(circuit_breaker::CircuitBreaker::new(dep, policy)))
            .clone()
    }

    /// 取（或创建）依赖的舱壁（并发上限取 failure_threshold 的 10 倍，至少 16）
    pub async fn bulkhead(&self, dep: &str) -> Arc<bulkhead::Bulkhead> {
        let policy = self.policy(dep).await;
        let permits = (policy.failure_threshold as usize * 10).max(16);
        let mut map = self.bulkheads.write().await;
        map.entry(dep.to_string())
            .or_insert_with(|| Arc::new(bulkhead::Bulkhead::new(dep, permits)))
            .clone()
    }

    /// 一站式调用：重试 + 熔断包裹（降级由调用方 `unwrap_or` / `or` 处理）。
    /// `dep` 是依赖名（熔断统计维度）；`f` 返回 `Err` 视为失败。
    pub async fn call<T, E, F, Fut>(&self, dep: &str, f: F) -> Result<T, ResilienceError<E>>
    where
        T: Send,
        E: std::fmt::Display + Send,
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<T, E>> + Send,
    {
        let policy = self.policy(dep).await;
        let breaker = self.breaker(dep).await;
        // 被熔断拒绝（Open）不该再消耗重试预算：熔断的意义就是快速失败
        retry::with_retry_when(&policy, || {
            let breaker = breaker.clone();
            let f = f();
            async move { breaker.call(f).await }
        }, |e: &ResilienceError<E>| !e.is_open())
        .await
        // with_retry 会把 Err 再包一层 Exhausted：拆开保持单层错误
        .map_err(|e| match e {
            ResilienceError::Exhausted { source, .. } => source,
            ResilienceError::Open { dep } => ResilienceError::Open { dep },
        })
    }
}

/// 弹性调用失败：区分被熔断拒绝与底层错误
#[derive(Debug, thiserror::Error)]
pub enum ResilienceError<E> {
    #[error("circuit `{dep}` is open (半开探测中，稍后重试)")]
    Open { dep: String },
    #[error("retries exhausted after {attempts} attempts: {source}")]
    Exhausted { attempts: u32, source: E },
}

impl<E> ResilienceError<E> {
    pub fn into_inner(self) -> E {
        match self {
            Self::Exhausted { source, .. } => source,
            Self::Open { .. } => panic!("Open has no inner error"),
        }
    }

    /// 是否被熔断拒绝（调用方可据此走降级路径）
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }
}

/// 时长别名（retry 退避计算用）
pub type Backoff = Duration;
