//! `Accept-Language` 协商。
//!
//! 按 q 值降序挑第一个「被支持」的语言；都不支持时回退到默认语言
//! （见 `docs/03` §4.21）。`*` 视为「任意」，直接取默认语言。

use super::locale::Locale;

/// 一条语言偏好。
#[derive(Debug, Clone)]
pub struct Preference {
    /// 语言标识（`*` 时为 `None`）。
    pub locale: Option<Locale>,
    /// 权重（0.0 ~ 1.0）。
    pub quality: f32,
}

/// 解析 `Accept-Language`：`zh-CN,zh;q=0.9,en;q=0.8`。
///
/// 无法解析的条目**跳过**（不报错——语言头不应让请求失败）。
#[must_use]
pub fn parse_accept_language(header: &str) -> Vec<Preference> {
    let mut prefs: Vec<Preference> = header
        .split(',')
        .filter_map(|part| {
            let mut segments = part.split(';');
            let tag = segments.next()?.trim();
            if tag.is_empty() {
                return None;
            }

            let quality = segments
                .find_map(|segment| {
                    let segment = segment.trim();
                    segment
                        .strip_prefix("q=")
                        .or_else(|| segment.strip_prefix("Q="))
                        .and_then(|q| q.trim().parse::<f32>().ok())
                })
                .unwrap_or(1.0);

            if !(0.0..=1.0).contains(&quality) {
                return None;
            }

            let locale = if tag == "*" {
                None
            } else {
                match Locale::parse(tag) {
                    Ok(locale) => Some(locale),
                    Err(_) => return None,
                }
            };

            Some(Preference { locale, quality })
        })
        .collect();

    // 稳定排序：q 相同时保持头部原始顺序（RFC 9110 建议）。
    prefs.sort_by(|a, b| {
        b.quality
            .partial_cmp(&a.quality)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    prefs
}

/// 协商出最终语言。
///
/// 1. 按 q 值顺序找第一个「与某个受支持语言匹配」的偏好；
/// 2. 若偏好是 `*`，直接返回 `default`；
/// 3. 全都不匹配时返回 `default`。
#[must_use]
pub fn negotiate(header: Option<&str>, supported: &[Locale], default: &Locale) -> Locale {
    let Some(header) = header.filter(|h| !h.trim().is_empty()) else {
        return default.clone();
    };

    for preference in parse_accept_language(header) {
        match &preference.locale {
            None => return default.clone(),
            Some(requested) => {
                if let Some(matched) = supported.iter().find(|s| requested.matches(s)) {
                    return matched.clone();
                }
            }
        }
    }

    default.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supported() -> Vec<Locale> {
        vec![
            Locale::parse("zh-CN").unwrap(),
            Locale::parse("en-US").unwrap(),
        ]
    }

    #[test]
    fn should_parse_quality_values() {
        let prefs = parse_accept_language("zh-CN,zh;q=0.9,en;q=0.8");
        assert_eq!(prefs.len(), 3);
        assert_eq!(prefs[0].locale.as_ref().unwrap().as_str(), "zh-CN");
        assert!((prefs[0].quality - 1.0).abs() < f32::EPSILON);
        assert_eq!(prefs[1].locale.as_ref().unwrap().as_str(), "zh");
        assert!((prefs[2].quality - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn should_sort_by_quality_desc() {
        let prefs = parse_accept_language("en;q=0.3,zh-CN;q=0.9");
        assert_eq!(prefs[0].locale.as_ref().unwrap().as_str(), "zh-CN");
    }

    #[test]
    fn should_skip_malformed_entries() {
        let prefs = parse_accept_language("!!,en;q=0.5,zh-CN;q=2");
        assert_eq!(prefs.len(), 1, "非法标签与越界 q 值应被跳过");
        assert_eq!(prefs[0].locale.as_ref().unwrap().as_str(), "en");
    }

    #[test]
    fn should_pick_first_supported_by_quality() {
        let default = Locale::parse("zh-CN").unwrap();
        let chosen = negotiate(Some("en-US,zh-CN;q=0.5"), &supported(), &default);
        assert_eq!(chosen.as_str(), "en-US");
    }

    #[test]
    fn should_match_language_only_tag() {
        let default = Locale::parse("en-US").unwrap();
        let chosen = negotiate(Some("zh;q=0.9"), &supported(), &default);
        assert_eq!(chosen.as_str(), "zh-CN", "zh 应命中受支持的 zh-CN");
    }

    #[test]
    fn should_fall_back_to_default() {
        let default = Locale::parse("zh-CN").unwrap();
        assert_eq!(
            negotiate(Some("fr-FR,de;q=0.5"), &supported(), &default).as_str(),
            "zh-CN"
        );
        assert_eq!(negotiate(None, &supported(), &default).as_str(), "zh-CN");
        assert_eq!(
            negotiate(Some("  "), &supported(), &default).as_str(),
            "zh-CN"
        );
    }

    #[test]
    fn should_return_default_for_wildcard() {
        let default = Locale::parse("zh-CN").unwrap();
        assert_eq!(
            negotiate(Some("*"), &supported(), &default).as_str(),
            "zh-CN"
        );
    }
}
