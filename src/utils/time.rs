//! 时间工具：统一 UTC 入库（i18n 文档 三·20 存储约定），展示层换算留给应用。
//!
//! handler / service 里统一从 `now_ms()` / `now_secs()` 取当前时间，测试时可替换
//! 的时钟暂未引入；需要确定性时间的用例自行注入 chrono::Utc::now() 的替代品。

use chrono::{DateTime, SecondsFormat, Utc}; // 引入 UTC 时间类型、秒精度格式与 Utc 时区

/// 当前 Unix 毫秒
pub fn now_ms() -> i64 { // 取当前 Unix 毫秒时间戳
    Utc::now().timestamp_millis() // 当前 UTC 时间的毫秒戳
}

/// 当前 Unix 秒
pub fn now_secs() -> i64 { // 取当前 Unix 秒时间戳
    Utc::now().timestamp() // 当前 UTC 时间的秒戳
}

/// 当前 UTC 时间
pub fn now_utc() -> DateTime<Utc> { // 取当前 UTC 时间对象
    Utc::now() // 直接返回当前 UTC 时间
}

/// RFC3339（毫秒精度，带 Z 后缀），日志与 API 输出统一格式
pub fn rfc3339(dt: DateTime<Utc>) -> String { // 把时间格式化为 RFC3339 字符串
    dt.to_rfc3339_opts(SecondsFormat::Millis, true) // 毫秒精度并用 Z 表示 UTC
}

/// 当前时间的 RFC3339 表示
pub fn now_rfc3339() -> String { // 取当前时间的 RFC3339 字符串
    rfc3339(Utc::now()) // 格式化当前 UTC 时间
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_stable() {
        let dt = DateTime::parse_from_rfc3339("2026-10-05T08:00:00.500Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(rfc3339(dt), "2026-10-05T08:00:00.500Z");
        assert_eq!(now_ms(), chrono::Utc::now().timestamp_millis());
    }
}
