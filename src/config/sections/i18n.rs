//! `[i18n]` 配置节：默认/支持语言、默认时区、货币、回退链（文档 三·20）。

use serde::{Deserialize, Serialize};

fn default_locale() -> String {
    "zh-CN".to_string()
}
fn default_timezone() -> String {
    "UTC".to_string()
}
fn default_currency() -> String {
    "CNY".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct I18nSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 默认语言（BCP-47，如 zh-CN）
    #[serde(default = "default_locale")]
    pub default_locale: String,
    /// 支持的语言列表；不在列表内的协商结果回落 default_locale
    #[serde(default = "default_supported")]
    pub supported: Vec<String>,
    /// 回退链：`zh-Hant` → `zh` → 默认语言（translator 缺键时逐级回退）
    #[serde(default)]
    pub fallbacks: Vec<String>,
    /// 默认 IANA 时区（如 Asia/Shanghai）
    #[serde(default = "default_timezone")]
    pub default_timezone: String,
    /// 默认货币（ISO 4217）
    #[serde(default = "default_currency")]
    pub default_currency: String,
    /// Fluent 翻译目录：`{dir}/{locale}.ftl`
    #[serde(default = "default_catalog_dir")]
    pub catalog_dir: String,
}

fn default_supported() -> Vec<String> {
    vec!["zh-CN".to_string(), "en".to_string()]
}
fn default_catalog_dir() -> String {
    "locales".to_string()
}

impl Default for I18nSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            default_locale: default_locale(),
            supported: default_supported(),
            fallbacks: Vec::new(),
            default_timezone: default_timezone(),
            default_currency: default_currency(),
            catalog_dir: default_catalog_dir(),
        }
    }
}
