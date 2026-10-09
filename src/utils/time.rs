//! 时间工具：统一 UTC 入库（i18n 文档 三·20 存储约定），展示层换算留给应用。
//!
//! handler / service 里统一从 `now_ms()` / `now_secs()` 取当前时间，测试时可替换
//! 的时钟暂未引入；需要确定性时间的用例自行注入 chrono::Utc::now() 的替代品。
//!
//! # 时区策略（重要）
//!
//! **框架不持有"默认时区"**。本模块只提供两样东西：
//!
//! 1. **瞬时点（永远 UTC）**：`now_ms` / `now_secs` / `now_utc` / `rfc3339`。
//!    这些是机器对齐用的绝对值，**任何情况下都不做时区换算**。
//! 2. **展示出口（走解析链）**：`resolve_display_tz` / `display_rfc3339` /
//!    `format_display`。用于"给人看"的输出。
//!
//! 解析链（[`resolve_display_tz`]）：
//!
//! ```text
//! 业务层已配置时区（[time].timezone，IANA）─► 用它
//! 业务层配置了非法时区名 ─────────────────► 返回 Err（调用方决定 fail-fast）
//! 业务层未配置 ──────────────────────────► 读取系统时区（TZ 环境变量 → OS 本地偏移）
//!                                              └ 系统时区不可用 ─► UTC（兜底）
//! ```
//!
//! **硬约束（勿改）**：DB 审计字段入库、雪花 ID 时间戳、JWT `iat`/`exp`、
//! CSRF token `exp`、各类 TTL 相对时长——全部必须保持 UTC / Unix 秒。
//! 改时区会破坏跨时区一致性与全局唯一性。

use std::sync::OnceLock; // 进程级一次性缓存（系统时区探测结果）

use chrono::{DateTime, FixedOffset, Offset, SecondsFormat, Utc}; // 引入 UTC 时间类型、固定偏移、Offset trait（提供 fix/suppress 等方法）、秒精度格式与 Utc 时区
use chrono_tz::Tz; // 引入 IANA 时区类型

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
///
/// 注意：这是**机器格式**，永远是 UTC。需要按展示时区渲染请用
/// [`display_rfc3339`]。
pub fn rfc3339(dt: DateTime<Utc>) -> String { // 把时间格式化为 RFC3339 字符串
    dt.to_rfc3339_opts(SecondsFormat::Millis, true) // 毫秒精度并用 Z 表示 UTC
}

/// 当前时间的 RFC3339 表示（UTC）
pub fn now_rfc3339() -> String { // 取当前时间的 RFC3339 字符串
    rfc3339(Utc::now()) // 格式化当前 UTC 时间
}

// ---------------------------------------------------------------------------
// 时区解析链
// ---------------------------------------------------------------------------

/// 时区解析失败：业务层配置了无法识别的时区名。
///
/// 框架不静默降级——静默降级会让"每天 3 点跑任务"悄悄变成"UTC 3 点跑"。
/// 调用方（启动期）应把它转成 fail-fast 错误。
#[derive(Debug, Clone, PartialEq, Eq)] // 派生调试/克隆/相等比较，便于测试断言
pub struct UnknownTimezone { // 未知时区错误
    /// 无法解析的时区名原文
    pub name: String, // 原始时区名，便于报错信息回显
}

impl std::fmt::Display for UnknownTimezone { // 实现展示，便于日志与错误串联
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 格式化输出
        write!( // 输出可操作的建议
            f, // 目标格式化器
            "unknown IANA timezone {:?}: 请使用形如 \"Asia/Shanghai\" / \"UTC\" 的 IANA 时区名，或留空以读取系统时区", // 错误文案
            self.name // 回显原始值
        )
    }
}

impl std::error::Error for UnknownTimezone {} // 接入标准错误体系

/// 解析"展示时区"。这是**全项目唯一**的时区解析入口。
///
/// - `configured`：业务层配置的时区名（如 `[time].timezone`）；`None`/空串 = 未配置
///
/// 返回 `Result`：已配置但非法时返回 [`UnknownTimezone`]，由调用方 fail-fast。
/// 未配置时依次回退"系统时区 → UTC"，**不会失败**。
pub fn resolve_display_tz(configured: Option<&str>) -> Result<Tz, UnknownTimezone> { // 按解析链得到展示时区
    match configured.map(str::trim).filter(|s| !s.is_empty()) { // 归一化：空白串视为未配置
        Some(name) => name.parse::<Tz>().map_err(|_| UnknownTimezone { name: name.to_string() }), // 已配置：解析 IANA 名，失败即报错
        None => Ok(system_tz()), // 未配置：回退系统时区（内部再兜底 UTC）
    }
}

/// 系统时区 → 兜底 UTC。**永不失败**（失败时降级为 UTC 并告警一次）。
///
/// 探测顺序：
/// 1. `TZ` 环境变量（可解析为 IANA 名时最可靠，且能正确处理 DST）；
/// 2. OS 本地偏移（`chrono::Local`，读 OS 时区设置）——用 `catch_unwind` 包裹，
///    因为部分容器环境缺 `/etc/localtime` 时 `Local` 可能 panic；
/// 3. UTC 兜底。
///
/// 结果在首次调用时探测并缓存（`OnceLock`）：避免每条日志都去读 OS 时区，
/// 也把潜在的 panic 风险集中到第一次调用。
pub fn system_tz() -> Tz { // 获取系统时区（含安全兜底）
    static CACHE: OnceLock<Tz> = OnceLock::new(); // 进程级缓存系统时区
    *CACHE.get_or_init(detect_system_tz) // 首次探测，之后复用
}

/// 实际的系统时区探测逻辑（可能 panic 的部分已被隔离）
fn detect_system_tz() -> Tz { // 探测系统时区
    // 1) TZ 环境变量优先：能解析成 IANA 名就用它（DST 正确）
    if let Ok(tz_name) = std::env::var("TZ") { // 读取 TZ 环境变量
        if let Ok(tz) = tz_name.trim().parse::<Tz>() { // 尝试解析为 IANA 时区
            return tz; // 解析成功直接采用
        }
    }

    // 2) OS 本地偏移：Local 在极端环境可能 panic，用 catch_unwind 隔离
    //    `Local::now().offset()` 已返回 `FixedOffset`，无需再 fix
    let offset = std::panic::catch_unwind(|| *chrono::Local::now().offset()).ok(); // 捕获 panic 取系统固定偏移
    if let Some(offset) = offset { // 成功拿到偏移
        if let Some(tz) = fixed_offset_to_tz(offset) { // 尝试匹配到等价的 IANA 时区
            return tz; // 匹配成功
        }
    }

    // 3) 兜底 UTC（唯一不会失败的分支）
    chrono_tz::UTC // 返回 IANA UTC 时区
}

/// 把固定偏移（如 +08:00）匹配到等价的 IANA 时区名（如 `Asia/Shanghai`）。
///
/// 固定偏移无法表达 DST，因此只能匹配"当前偏移与之相等"的时区；匹配不到时
/// 返回 `None`，由调用方兜底 UTC。要 DST 正确请显式配置 `[time].timezone`
/// 或设置 `TZ` 环境变量。
fn fixed_offset_to_tz(offset: FixedOffset) -> Option<Tz> { // 固定偏移 → IANA 时区
    const CANDIDATES: &[&str] = &[ // 候选时区列表
        "Asia/Shanghai", // 中国标准时间 +08:00
        "Asia/Hong_Kong", // 中国香港
        "Asia/Taipei", // 中国台湾
        "Asia/Tokyo", // 日本 +09:00
        "Asia/Seoul", // 韩国 +09:00
        "Asia/Singapore", // 新加坡 +08:00
        "Asia/Kolkata", // 印度 +05:30
        "Asia/Dubai", // 迪拜 +04:00
        "Europe/London", // 伦敦 +00:00/+01:00
        "Europe/Berlin", // 中欧 +01:00/+02:00
        "Europe/Moscow", // 莫斯科 +03:00
        "America/New_York", // 美东 -05:00/-04:00
        "America/Chicago", // 美中 -06:00/-05:00
        "America/Denver", // 美山 -07:00/-06:00
        "America/Los_Angeles", // 美西 -08:00/-07:00
        "Pacific/Auckland", // 新西兰 +12:00/+13:00
        "Australia/Sydney", // 悉尼 +10:00/+11:00
        "UTC", // 零时区
    ];
    let sample = Utc::now(); // 取当前时刻用于比较偏移
    CANDIDATES // 遍历候选
        .iter() // 迭代字符串
        .filter_map(|name| name.parse::<Tz>().ok()) // 解析为 Tz，忽略失败项
        .find(|tz| { // 找出当前偏移匹配的时区
            let cand = *sample.with_timezone(tz).offset(); // 候选时区在当前时刻的偏移
            cand.fix() == offset // 统一转成 FixedOffset 再比较
        })
}

// ---------------------------------------------------------------------------
// 展示出口（走解析链）
// ---------------------------------------------------------------------------

/// 按展示时区渲染 RFC3339（带偏移量，如 `2026-10-09T13:14:01.517+08:00`）。
///
/// `configured` 语义同 [`resolve_display_tz`]；解析失败时回落 UTC 渲染
/// （输出永远是合法时间串，不因时区问题丢日志/丢响应）。
pub fn display_rfc3339(dt: DateTime<Utc>, configured: Option<&str>) -> String { // 按展示时区格式化 RFC3339
    match resolve_display_tz(configured) { // 解析展示时区
        Ok(tz) => dt.with_timezone(&tz).to_rfc3339_opts(SecondsFormat::Millis, false), // 换算到目标时区（false = 输出偏移量而非 Z）
        Err(_) => rfc3339(dt), // 时区非法时回落 UTC 渲染，保证不丢输出
    }
}

/// 按展示时区 + 自定义格式渲染（如 `%Y-%m-%d %H:%M:%S`）。
///
/// 解析失败时回落 UTC 渲染。返回值恒为合法时间串。
pub fn format_display(dt: DateTime<Utc>, configured: Option<&str>, fmt: &str) -> String { // 按展示时区自定义格式渲染
    match resolve_display_tz(configured) { // 解析展示时区
        Ok(tz) => dt.with_timezone(&tz).format(fmt).to_string(), // 换算到目标时区后按格式渲染
        Err(_) => dt.format(fmt).to_string(), // 时区非法时按 UTC 渲染
    }
}

/// 当前时间的展示时区 RFC3339（便捷封装）
pub fn now_display_rfc3339(configured: Option<&str>) -> String { // 取当前时间的展示格式
    display_rfc3339(Utc::now(), configured) // 用当前 UTC 时间走展示出口
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone; // 测试中构造固定时刻需要 with_ymd_and_hms（TimeZone trait 方法）

    #[test]
    fn formats_are_stable() {
        let dt = DateTime::parse_from_rfc3339("2026-10-05T08:00:00.500Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(rfc3339(dt), "2026-10-05T08:00:00.500Z");
        assert_eq!(now_ms(), chrono::Utc::now().timestamp_millis());
    }

    #[test]
    fn instant_helpers_stay_utc() {
        // 瞬时点出口必须恒为 UTC，不受系统时区影响
        let dt = Utc.with_ymd_and_hms(2026, 10, 5, 8, 0, 0).unwrap();
        assert_eq!(rfc3339(dt), "2026-10-05T08:00:00.000Z");
    }

    #[test]
    fn resolve_configured_timezone() {
        let tz = resolve_display_tz(Some("Asia/Shanghai")).unwrap();
        assert_eq!(tz, chrono_tz::Asia::Shanghai);
    }

    #[test]
    fn resolve_blank_is_unconfigured() {
        // 空串 / 纯空白 → 视为未配置 → 系统时区（不会报错）
        assert!(resolve_display_tz(Some("")).is_ok());
        assert!(resolve_display_tz(Some("   ")).is_ok());
        assert!(resolve_display_tz(None).is_ok());
    }

    #[test]
    fn resolve_invalid_timezone_errors() {
        // 已配置但非法必须报错，绝不静默降级
        let err = resolve_display_tz(Some("Asia/Shanghi")).unwrap_err();
        assert_eq!(err.name, "Asia/Shanghi");
        assert!(err.to_string().contains("Asia/Shanghi"));
    }

    #[test]
    fn display_uses_configured_timezone() {
        let dt = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        assert_eq!(
            display_rfc3339(dt, Some("Asia/Shanghai")),
            "2026-10-05T20:00:00.000+08:00"
        );
        assert_eq!(display_rfc3339(dt, Some("UTC")), "2026-10-05T12:00:00.000+00:00");
    }

    #[test]
    fn display_invalid_falls_back_to_utc() {
        // 非法时区不 panic，回落 UTC 渲染
        let dt = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        assert_eq!(
            display_rfc3339(dt, Some("Nowhere/Xyz")),
            "2026-10-05T12:00:00.000Z"
        );
    }

    #[test]
    fn format_display_custom() {
        let dt = Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap();
        assert_eq!(
            format_display(dt, Some("Asia/Shanghai"), "%Y-%m-%d %H:%M:%S"),
            "2026-10-05 20:00:00"
        );
    }

    #[test]
    fn system_tz_never_panics() {
        // 系统时区探测必须永不 panic（极端环境兜底 UTC）
        let tz = system_tz();
        assert!(!tz.name().is_empty());
    }
}
