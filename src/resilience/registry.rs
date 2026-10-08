//! 弹性组件注册表 [`Registry`]：按依赖名持有各自的熔断器与舱壁（进程内共享）。

use std::collections::HashMap; // 引入 HashMap，按依赖名存放熔断器/舱壁
use std::sync::Arc; // 引入 Arc，跨线程共享策略与组件

use tokio::sync::RwLock; // 引入异步读写锁保护策略表

use crate::config::sections::{ResiliencePolicy, ResilienceSettings}; // 引入弹性策略与设置类型

use super::error::ResilienceError; // 引入弹性错误类型

/// 按依赖名持有各自的熔断器与舱壁（进程内共享；App 装配时创建，存于应用侧）
#[derive(Clone, Default)] // 派生 Clone（可共享）与 Default（空注册表）
pub struct Registry { // 弹性组件注册表
    settings: Arc<RwLock<ResilienceSettings>>, // 全局弹性设置（可热更新）
    breakers: Arc<RwLock<HashMap<String, Arc<super::circuit_breaker::CircuitBreaker>>>>, // 依赖名 → 熔断器
    bulkheads: Arc<RwLock<HashMap<String, Arc<super::bulkhead::Bulkhead>>>>, // 依赖名 → 舱壁
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
    pub async fn breaker(&self, dep: &str) -> Arc<super::circuit_breaker::CircuitBreaker> { // 获取或惰性创建熔断器
        let policy = self.policy(dep).await; // 先取该依赖的策略
        let mut map = self.breakers.write().await; // 取熔断器表写锁
        map.entry(dep.to_string()) // 以依赖名为键
            .or_insert_with(|| Arc::new(super::circuit_breaker::CircuitBreaker::new(dep, policy))) // 不存在则按策略创建
            .clone() // 克隆 Arc 返回
    }

    /// 取（或创建）依赖的舱壁（并发上限取 failure_threshold 的 10 倍，至少 16）
    pub async fn bulkhead(&self, dep: &str) -> Arc<super::bulkhead::Bulkhead> { // 获取或惰性创建舱壁
        let policy = self.policy(dep).await; // 先取该依赖的策略
        let permits = (policy.failure_threshold as usize * 10).max(16); // 由失败阈值推导并发上限
        let mut map = self.bulkheads.write().await; // 取舱壁表写锁
        map.entry(dep.to_string()) // 以依赖名为键
            .or_insert_with(|| Arc::new(super::bulkhead::Bulkhead::new(dep, permits))) // 不存在则创建
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
        super::retry::with_retry_when(&policy, || { // 按可重试判定包裹调用
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
