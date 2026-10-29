//! `[i18n]` 配置段（feature `i18n`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[i18n]`。

use serde::Deserialize;

/// 国际化配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct I18nSection {
    /// 默认语言（`Accept-Language` 协商不出结果时使用）。
    pub default_locale: String,
    /// 兜底语言。
    pub fallback_locale: String,
    /// Fluent 资源目录（`{dir}/{locale}.ftl`；文件名即受支持语言）。
    pub resource_dir: String,
    /// 展示时区（IANA，如 `Asia/Shanghai`）。
    pub timezone: String,
    /// 默认货币（ISO 4217，如 `CNY`）。
    pub currency: String,
}

impl Default for I18nSection {
    fn default() -> Self {
        Self {
            default_locale: "zh-CN".to_string(),
            fallback_locale: "en".to_string(),
            resource_dir: "locales".to_string(),
            timezone: "UTC".to_string(),
            currency: "CNY".to_string(),
        }
    }
}

impl I18nSection {
    /// 校验：语言标识与时区必须可解析。
    ///
    /// # Errors
    /// 语言标识或时区非法时返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        for (key, value) in [
            ("i18n.default_locale", &self.default_locale),
            ("i18n.fallback_locale", &self.fallback_locale),
        ] {
            if value
                .trim()
                .parse::<unic_langid::LanguageIdentifier>()
                .is_err()
            {
                return Err(format!("{key} = \"{value}\" 不是合法的语言标识"));
            }
        }
        if self.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(format!(
                "i18n.timezone = \"{}\" 不是合法的 IANA 时区",
                self.timezone
            ));
        }
        if self.resource_dir.trim().is_empty() {
            return Err("i18n.resource_dir 不能为空".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_documented_values() {
        let s = I18nSection::default();
        assert_eq!(s.default_locale, "zh-CN");
        assert_eq!(s.fallback_locale, "en");
        assert_eq!(s.resource_dir, "locales");
        assert_eq!(s.timezone, "UTC");
        assert_eq!(s.currency, "CNY");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_invalid_locale_and_timezone() {
        let bad_locale = I18nSection {
            default_locale: "!!".to_string(),
            ..I18nSection::default()
        };
        assert!(bad_locale.validate().is_err());

        let bad_tz = I18nSection {
            timezone: "Mars/Olympus".to_string(),
            ..I18nSection::default()
        };
        assert!(bad_tz.validate().is_err());

        let empty_dir = I18nSection {
            resource_dir: "  ".to_string(),
            ..I18nSection::default()
        };
        assert!(empty_dir.validate().is_err());
    }

    #[test]
    fn should_parse_overrides() {
        let json =
            r#"{ "default_locale": "en-US", "timezone": "Asia/Shanghai", "currency": "USD" }"#;
        let s: I18nSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.default_locale, "en-US");
        assert_eq!(s.timezone, "Asia/Shanghai");
        assert_eq!(s.currency, "USD");
        assert_eq!(s.resource_dir, "locales", "未给出的字段用默认值");
    }
}
