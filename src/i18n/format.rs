//! 时区 / 货币 / 日期格式化。
//!
//! **限制**（`docs/03` §4.21）：只做**时区换算 + 固定数字格式**；
//! 真正的 CLDR 本地化（如 `2026年10月17日`）需引入 `icu_decimal` / `icu_datetime`，
//! 待依赖方案确定后补齐。

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

/// 常见货币的符号与小数位。
fn currency_spec(currency: &str) -> (&'static str, usize) {
    match currency.to_ascii_uppercase().as_str() {
        "CNY" => ("¥", 2),
        "USD" => ("$", 2),
        "EUR" => ("€", 2),
        "GBP" => ("£", 2),
        "JPY" => ("¥", 0),
        "KRW" => ("₩", 0),
        _ => ("", 2),
    }
}

/// 格式化日期时间：`2026-10-17 23:01:00 CST`。
#[must_use]
pub fn format_datetime(dt: DateTime<Utc>, tz: Tz) -> String {
    dt.with_timezone(&tz)
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

/// 格式化日期：`2026-10-17`。
#[must_use]
pub fn format_date(dt: DateTime<Utc>, tz: Tz) -> String {
    dt.with_timezone(&tz).format("%Y-%m-%d").to_string()
}

/// 格式化货币：`¥12.30` / `$12.30` / `¥1230`（JPY 无小数）。
///
/// 未知货币码时退化为 `12.30 XXX`（不猜测符号）。
#[must_use]
pub fn format_currency(amount: f64, currency: &str) -> String {
    let (symbol, decimals) = currency_spec(currency);
    if symbol.is_empty() {
        return format!("{amount:.decimals$} {}", currency.to_ascii_uppercase());
    }
    format!("{symbol}{amount:.decimals$}")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn utc() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 15, 1, 0).unwrap()
    }

    #[test]
    fn should_format_datetime_in_timezone() {
        let tz: Tz = "Asia/Shanghai".parse().unwrap();
        assert_eq!(format_datetime(utc(), tz), "2026-10-17 23:01:00 CST");
        assert_eq!(format_date(utc(), tz), "2026-10-17");
    }

    #[test]
    fn should_format_utc_when_tz_is_utc() {
        let tz: Tz = "UTC".parse().unwrap();
        assert_eq!(format_date(utc(), tz), "2026-10-17");
    }

    #[test]
    fn should_format_known_currencies() {
        assert_eq!(format_currency(12.3, "CNY"), "¥12.30");
        assert_eq!(format_currency(12.3, "usd"), "$12.30");
        assert_eq!(format_currency(1230.0, "JPY"), "¥1230", "JPY 无小数位");
    }

    #[test]
    fn should_fall_back_for_unknown_currency() {
        assert_eq!(format_currency(12.3, "xau"), "12.30 XAU");
    }
}
