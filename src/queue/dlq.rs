//! 重试与死信判定（`[queue].retry` / `[queue].dlq`）。
//!
//! 消费失败（`nack`）后：未耗尽重试次数则**带退避重入队**，耗尽则**入死信队列**
//! （`[queue].dlq.enabled = true` 时；否则丢弃并记 warn）。
//!
//! 说明：`docs/08` 的 `[queue].retry` 只有 `max_attempts` 与 `backoff`，
//! **未定义退避基数**，故这里取 `100ms` 为基数（与 `[resilience].retry.base_delay`
//! 默认值一致），指数退避上限 `5s`。

use std::time::Duration;

use crate::config::QueueSection;
use crate::config::sections::RetryBackoff;

/// 退避基数（`docs/08` 未定义，取 100ms）。
const BACKOFF_BASE: Duration = Duration::from_millis(100);
/// 退避上限。
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// `nack` 之后的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// 重入队，并等待给定时长（退避）。
    Retry(Duration),
    /// 入死信队列。
    DeadLetter,
    /// 丢弃（DLQ 未启用且重试耗尽）。
    Drop,
}

/// 重试 / 死信策略。
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeadLetterPolicy {
    max_attempts: u32,
    backoff: RetryBackoff,
    dlq_enabled: bool,
}

impl DeadLetterPolicy {
    /// 由 `[queue]` 配置构造。
    pub(crate) fn from_config(config: &QueueSection) -> Self {
        Self {
            max_attempts: config.retry.max_attempts.max(1),
            backoff: config.retry.backoff,
            dlq_enabled: config.dlq.enabled,
        }
    }

    /// 依据**已发生的投递次数**（`attempts`，从 1 开始）决定处置。
    ///
    /// `attempts >= max_attempts` 即视为耗尽。
    pub(crate) fn decide(&self, attempts: u32) -> Decision {
        if attempts < self.max_attempts {
            return Decision::Retry(self.backoff_for(attempts));
        }
        if self.dlq_enabled {
            Decision::DeadLetter
        } else {
            Decision::Drop
        }
    }

    /// 退避时长：`fixed` 恒定；`exponential` 为 `base * 2^(attempts-1)`，封顶 `BACKOFF_MAX`。
    fn backoff_for(&self, attempts: u32) -> Duration {
        match self.backoff {
            RetryBackoff::Fixed => BACKOFF_BASE,
            RetryBackoff::Exponential => {
                let exponent = attempts.saturating_sub(1).min(6);
                BACKOFF_BASE
                    .saturating_mul(1u32 << exponent)
                    .min(BACKOFF_MAX)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_attempts: u32, backoff: RetryBackoff, dlq_enabled: bool) -> DeadLetterPolicy {
        DeadLetterPolicy {
            max_attempts,
            backoff,
            dlq_enabled,
        }
    }

    #[test]
    fn should_retry_until_attempts_exhausted() {
        let p = policy(3, RetryBackoff::Fixed, true);
        assert!(matches!(p.decide(1), Decision::Retry(_)));
        assert!(matches!(p.decide(2), Decision::Retry(_)));
        assert_eq!(p.decide(3), Decision::DeadLetter);
    }

    #[test]
    fn should_drop_when_dlq_disabled_and_exhausted() {
        let p = policy(1, RetryBackoff::Fixed, false);
        assert_eq!(p.decide(1), Decision::Drop);
    }

    #[test]
    fn should_use_constant_delay_for_fixed_backoff() {
        let p = policy(5, RetryBackoff::Fixed, true);
        assert_eq!(p.decide(1), Decision::Retry(BACKOFF_BASE));
        assert_eq!(p.decide(4), Decision::Retry(BACKOFF_BASE));
    }

    #[test]
    fn should_grow_exponentially_and_cap_for_exponential_backoff() {
        let p = policy(10, RetryBackoff::Exponential, true);
        assert_eq!(p.decide(1), Decision::Retry(BACKOFF_BASE));
        assert_eq!(
            p.decide(2),
            Decision::Retry(Duration::from_millis(200)),
            "第 2 次应为 base * 2"
        );
        assert_eq!(
            p.decide(4),
            Decision::Retry(Duration::from_millis(800)),
            "第 4 次应为 base * 8"
        );
        assert_eq!(p.decide(9), Decision::Retry(BACKOFF_MAX), "应封顶在 5s");
    }
}
