//! `[i18n]` 配置节：默认/支持语言、默认时区、货币、回退链（文档 三·20）。

use serde::{Deserialize, Serialize}; // 引入 serde 反序列化/序列化派生宏

fn default_locale() -> String { // 默认语言的默认值函数
    "zh-CN".to_string() // 默认简体中文
}
fn default_currency() -> String { // 默认货币的默认值函数
    "CNY".to_string() // 默认人民币
}

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 能力
pub struct I18nSettings { // 国际化配置结构
    #[serde(default)] // 缺失时用默认值
    pub enabled: bool, // 是否启用国际化
    /// 默认语言（BCP-47，如 zh-CN）
    #[serde(default = "default_locale")] // 缺失时用默认语言
    pub default_locale: String, // 默认语言标识
    /// 支持的语言列表；不在列表内的协商结果回落 default_locale
    #[serde(default = "default_supported")] // 缺失时用默认支持列表
    pub supported: Vec<String>, // 支持的语言列表
    /// 回退链：`zh-Hant` → `zh` → 默认语言（translator 缺键时逐级回退）
    #[serde(default)] // 缺失时用默认值
    pub fallbacks: Vec<String>, // 翻译缺失时的逐级回退语言链
    /// 默认 IANA 时区（如 Asia/Shanghai）。**非必填**：缺省（空串）= 未配置，
    /// 展示换算时回落 [`[time].timezone`](crate::config::sections::TimeSettings)
    /// → 系统时区 → UTC。
    ///
    /// 与 `[time].timezone` 的分工：本项是"终端用户视角"的默认时区（画页面用）；
    /// `[time].timezone` 是"进程/运维视角"的展示时区（日志用，不受用户语言影响）。
    #[serde(default)] // 缺失时为空串（未配置语义，不再默认 "UTC" 以架空虚置回退链）
    pub default_timezone: String, // 默认 IANA 时区（空 = 未配置）
    /// 默认货币（ISO 4217）
    #[serde(default = "default_currency")] // 缺失时用默认货币
    pub default_currency: String, // 默认货币代码
    /// Fluent 翻译目录：`{dir}/{locale}.ftl`
    #[serde(default = "default_catalog_dir")] // 缺失时用默认目录
    pub catalog_dir: String, // Fluent 翻译文件目录
}

fn default_supported() -> Vec<String> { // 默认支持语言的默认值函数
    vec!["zh-CN".to_string(), "en".to_string()] // 默认支持简体中文与英文
}
fn default_catalog_dir() -> String { // 默认翻译目录的默认值函数
    "locales".to_string() // 默认目录名为 locales
}

impl Default for I18nSettings { // 为国际化配置实现 Default
    fn default() -> Self { // 返回默认配置
        Self { // 构造默认配置
            enabled: false, // 默认不启用国际化
            default_locale: default_locale(), // 默认语言
            supported: default_supported(), // 默认支持语言列表
            fallbacks: Vec::new(), // 默认无回退链
            default_timezone: String::new(), // 默认未配置时区（回落 [time]/系统/UTC）
            default_currency: default_currency(), // 默认货币
            catalog_dir: default_catalog_dir(), // 默认翻译目录
        }
    }
}
