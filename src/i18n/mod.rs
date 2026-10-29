//! 国际化（feature `i18n`）。
//!
//! - [`Locale`]：语言标识（BCP 47 子集）
//! - [`Translator`]：`Fluent` 资源与 `t()` 翻译
//! - [`I18n`]：装配入口（`AppState.i18n`），负责加载资源与协商语言
//! - [`negotiate()`]：`Accept-Language` 协商（q 值排序 + 语言级匹配 + 默认回退）
//! - [`mod@format`]：时区换算 / 货币 / 日期格式化
//!
//! 文件划分：`locale` / `translator` / `negotiate` / `format` / `service`（装配与加载）/
//! `extractor`（`Locale` 提取器）。

mod extractor;
pub mod format;
pub mod locale;
pub mod negotiate;
mod service;
pub mod translator;

pub use format::{format_currency, format_date, format_datetime};
pub use locale::Locale;
pub use negotiate::{Preference, negotiate, parse_accept_language};
pub use service::I18n;
pub use translator::Translator;
