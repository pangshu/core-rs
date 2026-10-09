//! `[time]` 配置节：进程级展示时区（可选）。
//!
//! **设计定位**——框架不持有"默认时区"，本节的 `timezone` 是一个**可选入参**，
//! 由业务层决定是否填写，缺省即"未配置"（`None`，而非 `"UTC"`）。解析链：
//!
//! ```text
//! [time].timezone 已配置且合法（IANA）─► 用它
//! [time].timezone 已配置但非法 ────────► 启动期 fail-fast
//! [time].timezone 未配置 ─────────────► 读取系统时区
//!                                          └ 系统时区不可用 ─► UTC（兜底）
//! ```
//!
//! **消费方**：日志时间戳（`observability/logging.rs`）、cron 时区兜底
//! （`task/cron.rs`）、展示换算出口（`utils::time::display_rfc3339`）。
//!
//! **与 `[i18n].default_timezone` 的分工**：本节是"进程/运维视角"的展示时区
//! （日志不受用户语言影响）；i18n 那项是"终端用户视角"的默认时区，供业务画页面
//! 时使用。两者并存不冲突。

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

/// `[time]` 配置节
#[derive(Debug, Clone, Default, Serialize, Deserialize)] // 派生调试/克隆/默认值与 serde
pub struct TimeSettings {
    /// IANA 时区名（如 `Asia/Shanghai`、`America/New_York`）。
    ///
    /// **非必填**：缺省（不写 / 写空串 / 写显式 `null`）表示"业务层未配置"，
    /// 框架转去读取系统时区，系统时区不可用时兜底 UTC。
    /// 配置了非法值（拼错的 IANA 名）时在启动期 fail-fast，不静默降级。
    ///
    /// 环境变量覆盖：`APP_TIME__TIMEZONE=Asia/Shanghai`。
    #[serde(default)] // 缺失时为 None（"未配置"语义，不能用 "UTC" 冒充）
    pub timezone: Option<String>, // 可选 IANA 时区名
}

impl TimeSettings { // 为时区配置提供便捷方法
    /// 归一化后的时区名：空白串一律视为"未配置"（`None`）。
    /// 这样 `timezone = ""` 与完全不写该字段的语义一致。
    pub fn timezone_name(&self) -> Option<&str> { // 返回去除空白后的时区名
        self.timezone // 读取可选时区字段
            .as_deref() // 转成 Option<&str>
            .map(str::trim) // 去掉两端空白
            .filter(|s| !s.is_empty()) // 空串视为未配置
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_means_unconfigured() {
        // 整节缺失 / 字段缺失 → None（不是 "UTC"）
        assert_eq!(TimeSettings::default().timezone_name(), None);
    }

    #[test]
    fn blank_means_unconfigured() {
        let s = TimeSettings { timezone: Some("".into()) };
        assert_eq!(s.timezone_name(), None);
        let s = TimeSettings { timezone: Some("   ".into()) };
        assert_eq!(s.timezone_name(), None);
    }

    #[test]
    fn configured_value_is_trimmed() {
        let s = TimeSettings { timezone: Some("  Asia/Shanghai  ".into()) };
        assert_eq!(s.timezone_name(), Some("Asia/Shanghai"));
    }

    #[test]
    fn deserialize_absent_section_is_unconfigured() {
        // 整节缺失：Settings 里 [time] 未出现 → 未配置
        let cfg: crate::config::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.time.timezone_name(), None);
    }

    #[test]
    fn deserialize_explicit_timezone() {
        // 显式配置时区
        let cfg: crate::config::Settings =
            serde_json::from_str("{\"time\":{\"timezone\":\"Asia/Shanghai\"}}").unwrap();
        assert_eq!(cfg.time.timezone_name(), Some("Asia/Shanghai"));
    }

    #[test]
    fn deserialize_blank_section_is_unconfigured() {
        // 写了空的 [time] 节（或 timezone = null）也视为未配置
        let cfg: crate::config::Settings = serde_json::from_str("{\"time\":{\"timezone\":null}}").unwrap();
        assert_eq!(cfg.time.timezone_name(), None);
    }
}
