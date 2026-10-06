//! `[resilience]` 配置节：各依赖的熔断阈值、重试次数、降级开关（文档 三·7）。
//!
//! 按「依赖名 → 策略」组织：`[resilience.default]` 兜底，
//! `[resilience.deps.payment-api]` 按依赖覆盖。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn default_failure_threshold() -> u32 {
    5
}
fn default_open_secs() -> u64 {
    30
}
fn default_half_open_calls() -> u32 {
    1
}
fn default_backoff_ms() -> u64 {
    200
}
fn default_backoff_max_ms() -> u64 {
    10_000
}

/// 单个依赖的弹性策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResiliencePolicy {
    /// 熔断：连续失败次数达到阈值后打开
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// 熔断打开持续时长（秒），之后进入半开探测
    #[serde(default = "default_open_secs")]
    pub open_secs: u64,
    /// 半开状态允许的探测并发数
    #[serde(default = "default_half_open_calls")]
    pub half_open_max_calls: u32,
    /// 重试次数（0 = 不重试）
    #[serde(default)]
    pub max_retries: u32,
    /// 重试基础退避（毫秒），指数增长 + 抖动
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,
    /// 重试退避上限（毫秒）
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    /// 降级开关：允许 fallback 兜底
    #[serde(default)]
    pub fallback_enabled: bool,
}

impl Default for ResiliencePolicy {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            open_secs: default_open_secs(),
            half_open_max_calls: default_half_open_calls(),
            max_retries: 0,
            backoff_ms: default_backoff_ms(),
            backoff_max_ms: default_backoff_max_ms(),
            fallback_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResilienceSettings {
    /// 未显式配置的依赖使用该默认策略
    #[serde(default)]
    pub default: ResiliencePolicy,
    /// 按依赖名覆盖：`[resilience.deps."payment-api"]`
    #[serde(default)]
    pub deps: BTreeMap<String, ResiliencePolicy>,
}

impl ResilienceSettings {
    /// 取指定依赖的策略；未配置时回落 default
    pub fn policy(&self, dep: &str) -> ResiliencePolicy {
        self.deps.get(dep).cloned().unwrap_or_else(|| self.default.clone())
    }
}
