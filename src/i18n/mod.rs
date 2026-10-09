//! 国际化（feature = "i18n"，文档 三·20）：多语言 / 时区 / 货币。
//!
//! - [`locale`]：Locale 类型与解析、回退语言链（`zh-Hant` → `zh` → 默认语言）；
//! - [`translator`]：消息目录与查找（Fluent，`{dir}/{locale}.ftl`），支持缺省回退；
//! - [`format`]：日期 / 时区 / 货币 / 数字格式化（chrono-tz）；
//! - **与错误响应结合**：`AppError` 携带错误码，响应阶段按 locale 取消息模板
//!   翻译（[`translate_error_message`]）；
//! - **存储约定**：时间统一 UTC 入库、展示时按用户时区换算；金额以最小货币
//!   单位（分）存整数。
//! - **时区**：展示换算的时区走 `utils::time::resolve_display_tz` 解析链
//!   （`[i18n].default_timezone` / `[time].timezone` → 系统时区 → UTC），
//!   框架不预设默认时区。

pub mod format; // 导出日期/时区/货币/数字格式化子模块
pub mod locale; // 导出 Locale 类型与解析、回退链子模块
pub mod translator; // 导出消息目录加载与翻译器子模块

mod negotiate; // Accept-Language 协商

pub use locale::Locale; // 对外重导出 Locale，调用方无需关心子模块路径
pub use negotiate::negotiate; // 对外导出 Accept-Language 协商入口
pub use translator::{translate_error_message, Translator}; // 对外重导出翻译器与错误消息翻译
