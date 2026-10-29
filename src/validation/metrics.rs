//! 校验拒绝的计数指标（feature `metrics`）。
//!
//! 只用一个计数器：`validation_rejected_total`。
//!
//! 为什么不复用 `http_requests_total{status="400"}`：那个标签把「请求体解析失败」
//! （`10005`）、「Content-Type 不对」（`10004`）和「字段校验失败」（`10001`）混在一起，
//! 而这三者的处置完全不同——`10001` 突增意味着有人在 fuzz 参数或前端版本错配，
//! 需要单独告警。计数器是**进程级**的（与 `ValidationOptions` 同思路：
//! 校验入口拿不到 `AppState`），由 [`Metrics`](crate::telemetry::Metrics) 注册。

use std::sync::OnceLock;

use prometheus::{IntCounter, Opts};

/// 校验拒绝计数器（进程级，惰性创建）。
static REJECTED: OnceLock<IntCounter> = OnceLock::new();

/// 指标名（与 `/metrics` 输出一致）。
pub(crate) const REJECTED_TOTAL_NAME: &str = "validation_rejected_total";

/// 取计数器（首次调用时创建；`Metrics::new` 注册的就是这个实例）。
pub(crate) fn rejected_counter() -> &'static IntCounter {
    REJECTED.get_or_init(|| {
        IntCounter::with_opts(Opts::new(
            REJECTED_TOTAL_NAME,
            "输入校验拒绝次数（错误码 10001）",
        ))
        .expect("validation_rejected_total 定义合法")
    })
}

/// 记录一次校验拒绝。
pub(crate) fn record_rejection() {
    rejected_counter().inc();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_share_one_counter_instance() {
        // `OnceLock` 保证注册与自增用的是同一个实例，否则 `/metrics` 永远是 0
        let first = rejected_counter();
        let second = rejected_counter();
        assert!(std::ptr::eq(first, second));

        let before = first.get();
        record_rejection();
        assert_eq!(first.get(), before + 1);
    }
}
