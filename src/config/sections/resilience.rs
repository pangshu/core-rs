//! `[resilience]` 配置节：各依赖的熔断阈值、重试次数、降级开关（文档 三·7）。
//!
//! 按「依赖名 → 策略」组织：`[resilience.default]` 兜底，
//! `[resilience.deps.payment-api]` 按依赖覆盖。

use std::collections::BTreeMap; // 引入有序映射，按依赖名组织策略

use serde::{Deserialize, Serialize}; // 引入 serde 反序列化/序列化派生宏

fn default_failure_threshold() -> u32 { // 熔断失败阈值的默认值函数
    5 // 默认连续 5 次失败打开熔断
}
fn default_open_secs() -> u64 { // 熔断打开时长的默认值函数
    30 // 默认打开 30 秒
}
fn default_half_open_calls() -> u32 { // 半开探测并发的默认值函数
    1 // 默认半开允许 1 个探测
}
fn default_backoff_ms() -> u64 { // 重试基础退避的默认值函数
    200 // 默认退避 200 毫秒
}
fn default_backoff_max_ms() -> u64 { // 重试退避上限的默认值函数
    10_000 // 默认退避上限 10 秒
}

/// 单个依赖的弹性策略
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 能力
pub struct ResiliencePolicy { // 单个依赖的弹性策略结构
    /// 熔断：连续失败次数达到阈值后打开
    #[serde(default = "default_failure_threshold")] // 缺失时用默认阈值
    pub failure_threshold: u32, // 打开熔断所需的连续失败次数
    /// 熔断打开持续时长（秒），之后进入半开探测
    #[serde(default = "default_open_secs")] // 缺失时用默认时长
    pub open_secs: u64, // 熔断打开持续秒数
    /// 半开状态允许的探测并发数
    #[serde(default = "default_half_open_calls")] // 缺失时用默认并发数
    pub half_open_max_calls: u32, // 半开状态允许的探测并发数
    /// 重试次数（0 = 不重试）
    #[serde(default)] // 缺失时用默认值
    pub max_retries: u32, // 最大重试次数
    /// 重试基础退避（毫秒），指数增长 + 抖动
    #[serde(default = "default_backoff_ms")] // 缺失时用默认退避
    pub backoff_ms: u64, // 重试基础退避毫秒数
    /// 重试退避上限（毫秒）
    #[serde(default = "default_backoff_max_ms")] // 缺失时用默认上限
    pub backoff_max_ms: u64, // 退避毫秒数上限
    /// 降级开关：允许 fallback 兜底
    #[serde(default)] // 缺失时用默认值
    pub fallback_enabled: bool, // 是否启用降级兜底
}

impl Default for ResiliencePolicy { // 为弹性策略实现 Default
    fn default() -> Self { // 返回默认策略
        Self { // 构造默认策略
            failure_threshold: default_failure_threshold(), // 默认失败阈值
            open_secs: default_open_secs(), // 默认打开时长
            half_open_max_calls: default_half_open_calls(), // 默认半开并发
            max_retries: 0, // 默认不重试
            backoff_ms: default_backoff_ms(), // 默认基础退避
            backoff_max_ms: default_backoff_max_ms(), // 默认退避上限
            fallback_enabled: false, // 默认不启用降级
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)] // 派生调试/克隆/默认与 serde 能力
pub struct ResilienceSettings { // 弹性配置结构，按依赖组织策略
    /// 未显式配置的依赖使用该默认策略
    #[serde(default)] // 缺失时用默认值
    pub default: ResiliencePolicy, // 兜底默认策略
    /// 按依赖名覆盖：`[resilience.deps."payment-api"]`
    #[serde(default)] // 缺失时用默认值
    pub deps: BTreeMap<String, ResiliencePolicy>, // 按依赖名覆盖的策略表
}

impl ResilienceSettings { // 为弹性配置实现查询方法
    /// 取指定依赖的策略；未配置时回落 default
    pub fn policy(&self, dep: &str) -> ResiliencePolicy { // 取指定依赖的弹性策略
        self.deps.get(dep).cloned().unwrap_or_else(|| self.default.clone()) // 命中则克隆，否则回落默认策略
    }
}
