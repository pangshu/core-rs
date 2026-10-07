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

pub mod bulkhead; // 声明舱壁子模块（并发隔离）
pub mod circuit_breaker; // 声明熔断器子模块
pub mod fallback; // 声明降级子模块
pub mod retry; // 声明重试子模块

use std::collections::HashMap; // 引入 HashMap，按依赖名存放熔断器/舱壁
use std::sync::Arc; // 引入 Arc，跨线程共享策略与组件
use std::time::Duration; // 引入 Duration（Backoff 别名用）

use tokio::sync::RwLock; // 引入异步读写锁保护策略表

use crate::config::sections::{ResiliencePolicy, ResilienceSettings}; // 引入弹性策略与设置类型

/// 按依赖名持有各自的熔断器与舱壁（进程内共享；App 装配时创建，存于应用侧）
#[derive(Clone, Default)] // 派生 Clone（可共享）与 Default（空注册表）
pub struct Registry { // 弹性组件注册表
    settings: Arc<RwLock<ResilienceSettings>>, // 全局弹性设置（可热更新）
    breakers: Arc<RwLock<HashMap<String, Arc<circuit_breaker::CircuitBreaker>>>>, // 依赖名 → 熔断器
    bulkheads: Arc<RwLock<HashMap<String, Arc<bulkhead::Bulkhead>>>>, // 依赖名 → 舱壁
}

impl Registry {
    pub fn new(settings: ResilienceSettings) -> Self { // 以初始设置创建注册表
        Self {
            settings: Arc::new(RwLock::new(settings)), // 包装设置为可共享的读写锁
            breakers: Default::default(), // 熔断器表初始为空
            bulkheads: Default::default(), // 舱壁表初始为空
        }
    }

    /// 更新策略（可挂到 config watcher 订阅实现热更新）
    pub async fn update_settings(&self, settings: ResilienceSettings) { // 整体替换弹性设置
        *self.settings.write().await = settings; // 取写锁并写入新设置
    }

    /// 取依赖的当前策略
    pub async fn policy(&self, dep: &str) -> ResiliencePolicy { // 按依赖名解析生效策略
        self.settings.read().await.policy(dep) // 读锁下按 dep 解析（含 default 回退）
    }

    /// 取（或创建）依赖的熔断器
    pub async fn breaker(&self, dep: &str) -> Arc<circuit_breaker::CircuitBreaker> { // 获取或惰性创建熔断器
        let policy = self.policy(dep).await; // 先取该依赖的策略
        let mut map = self.breakers.write().await; // 取熔断器表写锁
        map.entry(dep.to_string()) // 以依赖名为键
            .or_insert_with(|| Arc::new(circuit_breaker::CircuitBreaker::new(dep, policy))) // 不存在则按策略创建
            .clone() // 克隆 Arc 返回
    }

    /// 取（或创建）依赖的舱壁（并发上限取 failure_threshold 的 10 倍，至少 16）
    pub async fn bulkhead(&self, dep: &str) -> Arc<bulkhead::Bulkhead> { // 获取或惰性创建舱壁
        let policy = self.policy(dep).await; // 先取该依赖的策略
        let permits = (policy.failure_threshold as usize * 10).max(16); // 由失败阈值推导并发上限
        let mut map = self.bulkheads.write().await; // 取舱壁表写锁
        map.entry(dep.to_string()) // 以依赖名为键
            .or_insert_with(|| Arc::new(bulkhead::Bulkhead::new(dep, permits))) // 不存在则创建
            .clone() // 克隆 Arc 返回
    }

    /// 一站式调用：重试 + 熔断包裹（降级由调用方 `unwrap_or` / `or` 处理）。
    /// `dep` 是依赖名（熔断统计维度）；`f` 返回 `Err` 视为失败。
    pub async fn call<T, E, F, Fut>(&self, dep: &str, f: F) -> Result<T, ResilienceError<E>> // 组合重试与熔断执行一次调用
    where
        T: Send, // 成功值需可跨线程
        E: std::fmt::Display + Send, // 错误需可打印且可跨线程
        F: Fn() -> Fut + Send + Sync, // 每次重试产出新的 Future
        Fut: std::future::Future<Output = Result<T, E>> + Send, // Future 可发送
    {
        let policy = self.policy(dep).await; // 取该依赖的策略
        let breaker = self.breaker(dep).await; // 取该依赖的熔断器
        // 被熔断拒绝（Open）不该再消耗重试预算：熔断的意义就是快速失败
        retry::with_retry_when(&policy, || { // 按可重试判定包裹调用
            let breaker = breaker.clone(); // 克隆熔断器进入闭包
            let f = f(); // 产出一次新的底层 Future
            async move { breaker.call(f).await } // 经熔断器执行该 Future
        }, |e: &ResilienceError<E>| !e.is_open()) // 仅当非熔断拒绝时才重试
        .await // 等待重试流程结束
        // with_retry 会把 Err 再包一层 Exhausted：拆开保持单层错误
        .map_err(|e| match e { // 拆解外层包装，保持错误层次简单
            ResilienceError::Exhausted { source, .. } => source, // 重试耗尽：取出底层错误
            ResilienceError::Open { dep } => ResilienceError::Open { dep }, // 熔断拒绝：原样保留
        })
    }
}

/// 弹性调用失败：区分被熔断拒绝与底层错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 并由 thiserror 实现 Error
pub enum ResilienceError<E> { // 弹性调用错误枚举
    #[error("circuit `{dep}` is open (半开探测中，稍后重试)")] // 该变体的 Display 文案
    Open { dep: String }, // 熔断打开被拒绝，携带依赖名
    #[error("retries exhausted after {attempts} attempts: {source}")] // 该变体的 Display 文案
    Exhausted { attempts: u32, source: E }, // 重试耗尽，携带尝试次数与底层错误
}

impl<E> ResilienceError<E> {
    pub fn into_inner(self) -> E { // 取出底层错误（仅 Exhausted 可用）
        match self {
            Self::Exhausted { source, .. } => source, // 重试耗尽：返回底层错误
            Self::Open { .. } => panic!("Open has no inner error"), // 熔断拒绝无底层错误，直接 panic
        }
    }

    /// 是否被熔断拒绝（调用方可据此走降级路径）
    pub fn is_open(&self) -> bool { // 判断是否为熔断拒绝
        matches!(self, Self::Open { .. }) // Open 变体返回 true
    }
}

/// 时长别名（retry 退避计算用）
pub type Backoff = Duration; // 把 Duration 别名为 Backoff，表达退避时长语义
