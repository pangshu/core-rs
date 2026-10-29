//! 告警日志节流。
//!
//! 限流层里的告警都发生在**异常路径**上（Redis 故障、键空间超限、来源不可知），
//! 而这些异常往往正是「每秒几万个请求」的场景。逐请求 `warn!` 会把日志系统
//! 变成瓶颈：Redis 越慢 → 日志越多 → 磁盘/采集越堵 → 服务更慢，形成正反馈。
//!
//! [`ThrottledWarn`] 把同一类告警压到「每 N 秒最多一条」，并在这条日志里带上
//! 被抑制的次数，既保住了可观测性，又不会放大故障。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 按时间窗节流的告警计数器。
#[derive(Debug)]
pub(crate) struct ThrottledWarn {
    every_secs: u64,
    /// 上次真正输出的时间（Unix 秒）
    last: AtomicU64,
    /// 自上次输出以来被抑制的条数
    suppressed: AtomicU64,
}

impl ThrottledWarn {
    /// 每 `every` 最多输出一条（至少 1 秒）。
    pub fn new(every: Duration) -> Self {
        Self {
            every_secs: every.as_secs().max(1),
            last: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// 记一次命中。
    ///
    /// 返回 `Some(被抑制条数)` 表示**本次应当输出**日志（附带这期间攒下的条数）；
    /// 返回 `None` 表示本次静默。
    pub fn hit(&self) -> Option<u64> {
        let now = u64::try_from(crate::utils::now_unix_secs()).unwrap_or(0);
        let last = self.last.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.every_secs {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // 抢占输出权：并发下只有一个线程能推进 `last`
        if self
            .last
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(self.suppressed.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_emit_first_then_suppress_within_window() {
        let warn = ThrottledWarn::new(Duration::from_secs(60));
        assert_eq!(warn.hit(), Some(0), "首次应输出");
        assert_eq!(warn.hit(), None, "窗口内应静默");
        assert_eq!(warn.hit(), None);
        // 窗口过后再输出一次，并带出被抑制的条数
        assert_eq!(warn.hit(), None);
        // 直接把窗口调成 0 无法表达，这里手动把 last 拨回去验证累计值
        warn.last.store(0, Ordering::Relaxed);
        assert_eq!(warn.hit(), Some(3), "应带出被抑制的 3 条");
    }
}
