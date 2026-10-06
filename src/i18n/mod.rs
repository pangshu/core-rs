//! 国际化（feature = "i18n"，文档 三·20）：多语言 / 时区 / 货币。
//!
//! - [`locale`]：Locale 类型与解析、回退语言链（`zh-Hant` → `zh` → 默认语言）；
//! - [`translator`]：消息目录与查找（Fluent，`{dir}/{locale}.ftl`），支持缺省回退；
//! - [`format`]：日期 / 时区 / 货币 / 数字格式化（chrono-tz）；
//! - **与错误响应结合**：`AppError` 携带错误码，响应阶段按 locale 取消息模板
//!   翻译（`translate_error_message`）；
//! - **存储约定**：时间统一 UTC 入库、展示时按用户时区换算；金额以最小货币
//!   单位（分）存整数。

pub mod format;
pub mod locale;
pub mod translator;

use crate::config::sections::I18nSettings;

pub use locale::Locale;
pub use translator::Translator;

/// Accept-Language 协商：在支持语言与回退链内选最佳匹配；
/// 无匹配时回落默认语言（i18n 关闭时等于默认语言）
pub fn negotiate(accept_language: &str, settings: &I18nSettings) -> Locale {
    let candidates = parse_accept_language(accept_language);
    for candidate in candidates {
        // 精确匹配（zh-CN）→ 主语言匹配（zh）→ 回退链
        if settings.supported.iter().any(|s| s.eq_ignore_ascii_case(&candidate)) {
            return Locale::parse(&candidate, settings);
        }
        let main = candidate.split('-').next().unwrap_or("").to_string();
        if settings.supported.iter().any(|s| s.eq_ignore_ascii_case(&main)) {
            return Locale::parse(&main, settings);
        }
    }
    Locale::default_for(settings)
}

/// `Accept-Language: zh-CN,zh;q=0.9,en;q=0.8` → 按权重降序的候选列表
fn parse_accept_language(header: &str) -> Vec<String> {
    let mut items: Vec<(f32, String)> = header
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let mut segments = part.split(';');
            let tag = segments.next()?.trim().to_string();
            let mut q = 1.0f32;
            for seg in segments {
                let seg = seg.trim();
                if let Some(v) = seg.strip_prefix("q=") {
                    // RFC 9110：q=0 表示明确不可接受；畸形 q 不能当成最高优先级，
                    // 一律按 0 处理（丢弃），而不是 unwrap_or(1.0)
                    q = v.parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0);
                }
            }
            if q <= 0.0 || tag.eq_ignore_ascii_case("*") {
                None
            } else {
                Some((q, tag))
            }
        })
        .collect();
    // sort_by 是稳定排序：等 q 保持出现顺序（RFC 建议的 tie-break）
    items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    items.into_iter().map(|(_, tag)| tag).collect()
}

/// `AppError` 消息翻译（响应阶段调用）：查 `error.{code}` 键，未命中返回 None
/// （调用方回退原始 message）
pub fn translate_error_message(
    translator: &Translator,
    locale: &Locale,
    code: i32,
    fallback: &str,
) -> Option<String> {
    let key = format!("error-{code}");
    translator.translate(locale, &key, &[]).or_else(|| {
        // 未命中回退原始消息（i18n 缺失不阻塞错误输出）
        Some(fallback.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        let settings = I18nSettings {
            enabled: true,
            default_locale: "zh-CN".into(),
            supported: vec!["zh-CN".into(), "en".into()],
            ..Default::default()
        };
        assert_eq!(negotiate("en-US,en;q=0.9", &settings).as_str(), "en");
        assert_eq!(negotiate("zh-Hant", &settings).as_str(), "zh-CN"); // zh 不在支持列表，回默认
        assert_eq!(negotiate("", &settings).as_str(), "zh-CN");
        assert_eq!(negotiate("fr,de;q=0.5", &settings).as_str(), "zh-CN");
    }
}
