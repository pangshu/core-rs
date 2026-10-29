//! 时间与时区工具。

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

/// 返回当前 UTC 时间。
#[must_use]
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// 当前 Unix 时间戳（秒）。
#[must_use]
pub fn now_unix_secs() -> i64 {
    now_utc().timestamp()
}

/// 解析 IANA 时区名（如 `Asia/Shanghai`）。
///
/// # Errors
/// 名称非法时返回 [`chrono_tz::ParseError`]。
pub fn parse_timezone(name: &str) -> Result<Tz, chrono_tz::ParseError> {
    name.parse::<Tz>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_iana_timezone() {
        assert!(parse_timezone("Asia/Shanghai").is_ok());
        assert!(parse_timezone("Not/AZone").is_err());
    }
}
