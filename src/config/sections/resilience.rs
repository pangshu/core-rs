//! `[resilience]` 配置段（feature `resilience`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[resilience]`。
//! 全部作用于**出站调用**；入站超时见 `[server].request_timeout`。

use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::RetryBackoff;
use crate::config::sections::de::de_duration;

/// 出站重试配置。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ResilienceRetrySection {
    /// **出站调用**重试次数上限。
    pub max_attempts: u32,
    /// 退避策略。
    pub backoff: RetryBackoff,
    /// 基础退避。
    #[serde(deserialize_with = "de_duration")]
    pub base_delay: Duration,
    /// 退避上限。
    #[serde(deserialize_with = "de_duration")]
    pub max_delay: Duration,
    /// 被重试的调用是否**幂等**（默认 `false`）。
    ///
    /// 超时（`10006`）意味着「结果未知」而不是「确定失败」：非幂等调用（下单、
    /// 支付、发券）重试会在下游重复执行。因此默认 `false` 时**不重试超时**，
    /// 只重试确定失败的上游错误码；确认调用幂等（或已带幂等键）后再打开。
    pub idempotent: bool,
}

impl Default for ResilienceRetrySection {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: RetryBackoff::Exponential,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            idempotent: false,
        }
    }
}

/// 熔断配置。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct CircuitBreakerSection {
    /// 连续失败阈值。
    pub failure_threshold: u32,
    /// 熔断持续时长。
    #[serde(deserialize_with = "de_duration")]
    pub open_duration: Duration,
    /// 半开探测并发数。
    pub half_open_requests: u32,
}

impl Default for CircuitBreakerSection {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_duration: Duration::from_secs(30),
            half_open_requests: 1,
        }
    }
}

/// 舱壁配置。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct BulkheadSection {
    /// 并发出站上限。
    pub max_concurrent: u32,
}

impl Default for BulkheadSection {
    fn default() -> Self {
        Self {
            max_concurrent: 100,
        }
    }
}

/// 出站超时配置。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct TimeoutSection {
    /// **出站调用**默认超时（入站超时见 `[server].request_timeout`）。
    #[serde(deserialize_with = "de_duration")]
    pub default: Duration,
}

impl Default for TimeoutSection {
    fn default() -> Self {
        Self {
            default: Duration::from_secs(10),
        }
    }
}

/// 韧性配置段。
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(default)]
#[non_exhaustive]
pub struct ResilienceSection {
    /// 出站重试。
    pub retry: ResilienceRetrySection,
    /// 熔断。
    pub circuit_breaker: CircuitBreakerSection,
    /// 舱壁。
    pub bulkhead: BulkheadSection,
    /// 出站超时。
    pub timeout: TimeoutSection,
}

/// 重试次数上限。
///
/// 没有上限的 `max_attempts` 等于「把故障放大 N 倍」：下游已经过载时，
/// 重试是压垮它的最后一根稻草。10 次已经远超任何合理的瞬时抖动窗口。
const MAX_ATTEMPTS_LIMIT: u32 = 10;

impl ResilienceSection {
    /// 启动期校验（fail-fast）。
    ///
    /// 这里挡的都是「配了但等于没配」的取值——它们不会报错，只会让韧性静默失效：
    ///
    /// - `timeout.default = 0` → 出站超时被彻底关闭（`with_timeout` 遇 0 直接透传）
    /// - `max_delay = 0` → 退避恒为 0，重试变成紧循环
    /// - `open_duration = 0` → 熔断器每次请求都立刻转半开，形同不存在
    ///
    /// # Errors
    /// 任一字段越界时返回可读的原因。
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_ATTEMPTS_LIMIT).contains(&self.retry.max_attempts) {
            return Err(format!(
                "[resilience].retry.max_attempts 必须在 1..={MAX_ATTEMPTS_LIMIT}（当前 {}）：\
                 没有上限的重试会把下游故障放大成雪崩",
                self.retry.max_attempts
            ));
        }
        if self.retry.max_delay.is_zero() {
            return Err(
                "[resilience].retry.max_delay 必须大于 0：为 0 时退避恒为 0，重试变成紧循环"
                    .to_string(),
            );
        }
        if self.circuit_breaker.failure_threshold == 0 {
            return Err("[resilience].circuit_breaker.failure_threshold 必须大于 0".to_string());
        }
        if self.circuit_breaker.open_duration.is_zero() {
            return Err("[resilience].circuit_breaker.open_duration 必须大于 0：\
                 为 0 时熔断器每次请求都立刻转半开，形同不存在"
                .to_string());
        }
        if self.circuit_breaker.half_open_requests == 0 {
            return Err("[resilience].circuit_breaker.half_open_requests 必须大于 0".to_string());
        }
        if self.bulkhead.max_concurrent == 0 {
            return Err("[resilience].bulkhead.max_concurrent 必须大于 0".to_string());
        }
        if self.timeout.default.is_zero() {
            return Err(
                "[resilience].timeout.default 必须大于 0：为 0 会**关闭**出站超时，\
                 出站调用可以无限挂起（显式不要超时请改用不套 with_timeout 的调用路径）"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_documented_values() {
        let s = ResilienceSection::default();
        assert_eq!(s.retry.max_attempts, 3);
        assert_eq!(s.retry.backoff, RetryBackoff::Exponential);
        assert_eq!(s.retry.base_delay, Duration::from_millis(100));
        assert_eq!(s.retry.max_delay, Duration::from_secs(5));
        assert!(!s.retry.idempotent, "默认按非幂等处理：不重试超时");
        assert_eq!(s.circuit_breaker.failure_threshold, 5);
        assert_eq!(s.circuit_breaker.open_duration, Duration::from_secs(30));
        assert_eq!(s.circuit_breaker.half_open_requests, 1);
        assert_eq!(s.bulkhead.max_concurrent, 100);
        assert_eq!(s.timeout.default, Duration::from_secs(10));
        assert!(s.validate().is_ok(), "默认值必须能通过校验");
    }

    #[test]
    fn should_parse_nested_fields() {
        let json = r#"
            {
                "retry": { "max_attempts": 5, "backoff": "fixed", "base_delay": "50ms", "idempotent": true },
                "circuit_breaker": { "failure_threshold": 2, "open_duration": "1m" },
                "bulkhead": { "max_concurrent": 8 },
                "timeout": { "default": "3s" }
            }
        "#;
        let s: ResilienceSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.retry.max_attempts, 5);
        assert_eq!(s.retry.backoff, RetryBackoff::Fixed);
        assert_eq!(s.retry.base_delay, Duration::from_millis(50));
        assert!(s.retry.idempotent);
        assert_eq!(s.circuit_breaker.failure_threshold, 2);
        assert_eq!(s.circuit_breaker.open_duration, Duration::from_secs(60));
        assert_eq!(s.bulkhead.max_concurrent, 8);
        assert_eq!(s.timeout.default, Duration::from_secs(3));
    }

    #[test]
    fn should_reject_zero_timeout() {
        let mut s = ResilienceSection::default();
        s.timeout.default = Duration::ZERO;
        let error = s.validate().unwrap_err();
        assert!(error.contains("timeout.default"), "{error}");
    }

    #[test]
    fn should_reject_unbounded_retry() {
        let mut s = ResilienceSection::default();
        s.retry.max_attempts = 0;
        assert!(s.validate().is_err());
        s.retry.max_attempts = MAX_ATTEMPTS_LIMIT + 1;
        assert!(s.validate().is_err(), "重试次数必须有上限");
        s.retry.max_attempts = MAX_ATTEMPTS_LIMIT;
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_zero_backoff() {
        let mut s = ResilienceSection::default();
        s.retry.max_delay = Duration::ZERO;
        assert!(s.validate().is_err(), "退避上限为 0 会让重试变成紧循环");
    }

    #[test]
    fn should_reject_degenerate_circuit_breaker() {
        let mut s = ResilienceSection::default();
        s.circuit_breaker.failure_threshold = 0;
        assert!(s.validate().is_err());

        let mut s = ResilienceSection::default();
        s.circuit_breaker.open_duration = Duration::ZERO;
        assert!(s.validate().is_err(), "open_duration 为 0 等于没有熔断");

        let mut s = ResilienceSection::default();
        s.circuit_breaker.half_open_requests = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_zero_bulkhead() {
        let mut s = ResilienceSection::default();
        s.bulkhead.max_concurrent = 0;
        assert!(s.validate().is_err());
    }
}
