//! 地理信息模型。

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::Serialize;

/// 城市级地理信息（字段全部可空——MMDB 记录并不保证完整）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GeoInfo {
    /// 查询的 IP。
    pub ip: Option<IpAddr>,
    /// 国家名（按多语言字段的语言优先级挑选）。
    pub country: Option<String>,
    /// 国家代码（ISO 3166-1 alpha-2）。
    pub country_code: Option<String>,
    /// 大洲代码（如 `AS`）。
    pub continent_code: Option<String>,
    /// 一级行政区名（省 / 州）。
    pub region: Option<String>,
    /// 城市名。
    pub city: Option<String>,
    /// 邮编。
    pub postal_code: Option<String>,
    /// 纬度。
    pub latitude: Option<f64>,
    /// 经度。
    pub longitude: Option<f64>,
    /// IANA 时区（如 `Asia/Shanghai`）。
    pub timezone: Option<String>,
    /// 自治系统号。
    pub asn: Option<u32>,
    /// 运营商 / 组织。
    pub organization: Option<String>,
}

/// 从 MMDB 的多语言名表里挑一个名字。
///
/// 优先级：`zh-CN` → `zh` → `en` → 字典序第一个（保证结果稳定）。
pub(crate) fn pick_name(names: &BTreeMap<&str, &str>) -> Option<String> {
    for key in ["zh-CN", "zh", "en"] {
        if let Some(name) = names.get(key) {
            return Some((*name).to_string());
        }
    }
    names.values().next().map(|name| (*name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_prefer_chinese_then_english() {
        let mut names = BTreeMap::new();
        names.insert("en", "China");
        names.insert("zh-CN", "中国");
        assert_eq!(pick_name(&names).as_deref(), Some("中国"));

        let mut english_only = BTreeMap::new();
        english_only.insert("en", "China");
        assert_eq!(pick_name(&english_only).as_deref(), Some("China"));
    }

    #[test]
    fn should_fall_back_to_any_available_name() {
        let mut names = BTreeMap::new();
        names.insert("ja", "中国");
        assert_eq!(pick_name(&names).as_deref(), Some("中国"));
    }

    #[test]
    fn should_return_none_for_empty_names() {
        assert!(pick_name(&BTreeMap::new()).is_none());
    }
}
