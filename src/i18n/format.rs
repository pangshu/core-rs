//! 日期 / 时区 / 货币 / 数字格式化（chrono-tz，文档 三·20）。
//!
//! 存储约定：时间统一 UTC 入库、展示时按用户时区换算；金额以最小货币单位
//! （分）存整数，避免浮点误差。

use chrono::{DateTime, Utc}; // 引入 chrono 的 UTC 时间类型

/// UTC 时间换算到目标 IANA 时区并格式化（`%Y-%m-%d %H:%M:%S`）。
/// 未知时区回落 UTC。
pub fn format_datetime(dt: DateTime<Utc>, timezone: &str) -> String { // 按目标时区格式化 UTC 时间
    match timezone.parse::<chrono_tz::Tz>() { // 解析 IANA 时区名
        Ok(tz) => dt // 解析成功：换算到该时区
            .with_timezone(&tz) // 切换到目标时区
            .format("%Y-%m-%d %H:%M:%S") // 按固定格式渲染
            .to_string(), // 转成字符串
        Err(_) => dt.format("%Y-%m-%d %H:%M:%S").to_string(), // 解析失败：按 UTC 直接格式化
    }
}

/// 分（最小货币单位）→ 金额字符串（两位小数），如 `12345` → `"123.45"`。
/// 千分位与货币符号由前端/展示层追加（避免不同 locale 的符号约定纠缠）。
pub fn format_minor_units(minor: i64) -> String { // 最小货币单位转金额字符串
    let sign = if minor < 0 { "-" } else { "" }; // 负数取负号
    let abs = minor.unsigned_abs(); // 取绝对值（避免 i64::MIN 溢出）
    format!("{sign}{}.{:02}", abs / 100, abs % 100) // 整数部分 + 两位小数部分
}

/// 数字千分位分组：`1234567` → `"1,234,567"`
pub fn format_number(n: i64) -> String { // 整数加千分位分隔
    let s = n.unsigned_abs().to_string(); // 取绝对值字符串（避免溢出）
    let sign = if n < 0 { "-" } else { "" }; // 负数取负号
    let mut out = String::with_capacity(s.len() + s.len() / 3); // 预分配输出缓冲（含逗号位）
    for (i, c) in s.chars().enumerate() { // 逐位遍历数字字符
        if i > 0 && (s.len() - i) % 3 == 0 { // 非首位且剩余位数为 3 的倍数时
            out.push(','); // 插入千分位逗号
        }
        out.push(c); // 写入当前数字字符
    }
    format!("{sign}{out}") // 拼接符号与结果
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timezone_formatting() {
        let dt = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        assert_eq!(format_datetime(dt, "Asia/Shanghai"), "2026-10-05 20:00:00");
        assert_eq!(format_datetime(dt, "UTC"), "2026-10-05 12:00:00");
        assert_eq!(format_datetime(dt, "Nowhere/Xyz"), "2026-10-05 12:00:00");
    }

    #[test]
    fn money_and_number() {
        assert_eq!(format_minor_units(12345), "123.45");
        assert_eq!(format_minor_units(-99), "-0.99");
        assert_eq!(format_number(1_234_567), "1,234,567");
        assert_eq!(format_number(-999), "-999");
    }
}
