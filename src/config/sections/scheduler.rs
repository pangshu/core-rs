//! `[scheduler]` 配置段（feature `task`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[scheduler]`。

use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::de::de_duration;

/// 选主锁获取失败时的策略。
///
/// 只在 `distributed.enabled = true` 时有意义：选主锁是「不双跑」的唯一保证，
/// 拿不到锁时**默认跳过**（宁可漏跑一次，也不让两个实例同时执行安全任务）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LockErrorPolicy {
    /// 跳过本次触发（默认）：fail-closed，漏跑一次。
    #[default]
    Skip,
    /// 照常执行：fail-open，**可能与其他实例重复执行**，仅在任务本身幂等时使用。
    Run,
}

impl LockErrorPolicy {
    /// 配置值与日志里的短名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Run => "run",
        }
    }
}

/// 分布式选主配置。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DistributedSection {
    /// 是否多实例选主（需 `[cache].backend = "redis"`）。
    pub enabled: bool,
    /// 选主锁 TTL；执行期间由看门狗按 `ttl / 3` 周期续期。
    #[serde(deserialize_with = "de_duration")]
    pub lock_ttl: Duration,
    /// 选主锁获取失败时的策略。
    pub on_lock_error: LockErrorPolicy,
}

impl Default for DistributedSection {
    fn default() -> Self {
        Self {
            enabled: false,
            lock_ttl: Duration::from_secs(30),
            on_lock_error: LockErrorPolicy::Skip,
        }
    }
}

/// 调度配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct SchedulerSection {
    /// 是否启用调度。
    pub enabled: bool,
    /// cron 解析用的 IANA 时区。
    pub timezone: String,
    /// 分布式选主。
    pub distributed: DistributedSection,
    /// 单次执行的超时上限；`0` 表示不限制（**不建议**：卡死任务会持续占锁并堆积）。
    #[serde(deserialize_with = "de_duration")]
    pub job_timeout: Duration,
    /// 关闭时等待在途任务收敛的上限，超时后强制 abort。
    #[serde(deserialize_with = "de_duration")]
    pub drain_timeout: Duration,
}

impl Default for SchedulerSection {
    fn default() -> Self {
        Self {
            enabled: true,
            timezone: "UTC".to_string(),
            distributed: DistributedSection::default(),
            job_timeout: Duration::from_secs(300),
            drain_timeout: Duration::from_secs(30),
        }
    }
}

impl SchedulerSection {
    /// 校验：时区可解析、锁 TTL 非零且足以支撑续期、drain 超时非零。
    ///
    /// 注：`distributed.enabled` 与 `[cache].backend` 的**跨段**校验在
    /// `AppConfig::validate` 中完成（见 `docs/02` §12.1）。
    ///
    /// # Errors
    /// 参数非法时返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        if self.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(format!(
                "scheduler.timezone = \"{}\" 不是合法的 IANA 时区",
                self.timezone
            ));
        }
        if self.distributed.lock_ttl.is_zero() {
            return Err("scheduler.distributed.lock_ttl 必须大于 0".to_string());
        }
        // 续期周期是 ttl/3：ttl 太小会让看门狗自己成为负载，且极易在 GC/调度抖动下失锁
        if self.distributed.enabled && self.distributed.lock_ttl < Duration::from_secs(1) {
            return Err("scheduler.distributed.lock_ttl 至少 1s（续期周期为其 1/3）".to_string());
        }
        if self.drain_timeout.is_zero() {
            return Err(
                "scheduler.drain_timeout 必须大于 0（0 等于关闭时不等待在途任务）".to_string(),
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
        let s = SchedulerSection::default();
        assert!(s.enabled);
        assert_eq!(s.timezone, "UTC");
        assert!(!s.distributed.enabled);
        assert_eq!(s.distributed.lock_ttl, Duration::from_secs(30));
        assert_eq!(s.distributed.on_lock_error, LockErrorPolicy::Skip);
        assert_eq!(s.job_timeout, Duration::from_secs(300));
        assert_eq!(s.drain_timeout, Duration::from_secs(30));
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_invalid_timezone_and_zero_ttl() {
        let bad_tz = SchedulerSection {
            timezone: "Mars/Olympus".to_string(),
            ..SchedulerSection::default()
        };
        assert!(bad_tz.validate().is_err());

        let zero_ttl = SchedulerSection {
            distributed: DistributedSection {
                enabled: true,
                lock_ttl: Duration::ZERO,
                on_lock_error: LockErrorPolicy::Skip,
            },
            ..SchedulerSection::default()
        };
        assert!(zero_ttl.validate().is_err());
    }

    #[test]
    fn should_reject_too_small_lock_ttl_when_distributed() {
        let section = SchedulerSection {
            distributed: DistributedSection {
                enabled: true,
                lock_ttl: Duration::from_millis(500),
                on_lock_error: LockErrorPolicy::Skip,
            },
            ..SchedulerSection::default()
        };
        assert!(section.validate().is_err(), "续期周期过密应 fail-fast");

        // 未开启 distributed 时不校验该下限（内存锁不续期）
        let disabled = SchedulerSection {
            distributed: DistributedSection {
                enabled: false,
                lock_ttl: Duration::from_millis(500),
                on_lock_error: LockErrorPolicy::Skip,
            },
            ..SchedulerSection::default()
        };
        assert!(disabled.validate().is_ok());
    }

    #[test]
    fn should_reject_zero_drain_timeout() {
        let section = SchedulerSection {
            drain_timeout: Duration::ZERO,
            ..SchedulerSection::default()
        };
        assert!(section.validate().is_err());
    }

    #[test]
    fn should_parse_overrides() {
        let json = r#"{ "enabled": false, "timezone": "Asia/Shanghai", "distributed": { "enabled": true, "lock_ttl": "1m", "on_lock_error": "run" }, "job_timeout": "2m", "drain_timeout": 5 }"#;
        let s: SchedulerSection = serde_json::from_str(json).unwrap();
        assert!(!s.enabled);
        assert_eq!(s.timezone, "Asia/Shanghai");
        assert!(s.distributed.enabled);
        assert_eq!(s.distributed.lock_ttl, Duration::from_secs(60));
        assert_eq!(s.distributed.on_lock_error, LockErrorPolicy::Run);
        assert_eq!(s.job_timeout, Duration::from_secs(120));
        assert_eq!(s.drain_timeout, Duration::from_secs(5));
    }

    #[test]
    fn should_fall_back_to_defaults_for_missing_fields() {
        let s: SchedulerSection = serde_json::from_str(r#"{ "enabled": true }"#).unwrap();
        assert_eq!(s.job_timeout, Duration::from_secs(300));
        assert_eq!(s.drain_timeout, Duration::from_secs(30));
        assert_eq!(s.distributed.on_lock_error, LockErrorPolicy::Skip);
    }
}
