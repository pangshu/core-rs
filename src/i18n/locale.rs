//! 语言标识（`unic-langid`）。
//!
//! 只做**标识与匹配**，不含任何翻译内容（见 `docs/03` §4.21）。

use std::str::FromStr;

use unic_langid::LanguageIdentifier;

use crate::errors::{AppError, AppResult, ErrorCode};

/// 语言标识（如 `zh-CN` / `en`）。
///
/// 同时是 axum 提取器：直接作为 handler 参数即可拿到**协商后**的语言
/// （见 [`Locale::from_request_parts`](axum::extract::FromRequestParts)）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locale {
    /// 规范化后的原始串（用于文件名 `{locale}.ftl` 与输出）。
    raw: String,
    /// 解析后的标识（用于匹配）。
    id: LanguageIdentifier,
}

impl Locale {
    /// 解析语言标识。
    ///
    /// # Errors
    /// 非法标识返回 `10001`（`INVALID_PARAM`）。
    pub fn parse(raw: &str) -> AppResult<Self> {
        let trimmed = raw.trim();
        let id: LanguageIdentifier = trimmed.parse().map_err(|_| {
            AppError::biz(ErrorCode::INVALID_PARAM, format!("非法语言标识：{trimmed}"))
        })?;
        // 用 `id.to_string()` 归一化（`zh-cn` → `zh-CN`）。
        Ok(Self {
            raw: id.to_string(),
            id,
        })
    }

    /// 规范化后的标识串。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// 主语言子标签（`zh-CN` → `zh`）。
    #[must_use]
    pub fn language(&self) -> &str {
        self.id.language.as_str()
    }

    /// 是否与另一标识「语言相同」。
    ///
    /// 规则：主语言必须相同；地区任一为空即视为匹配（`zh` ≈ `zh-CN`），
    /// 否则地区必须相同。
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.language() == other.language()
            && (self.id.region.is_none()
                || other.id.region.is_none()
                || self.id.region == other.id.region)
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for Locale {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_normalize_case() {
        assert_eq!(Locale::parse("zh-cn").unwrap().as_str(), "zh-CN");
        assert_eq!(Locale::parse(" en-us ").unwrap().as_str(), "en-US");
    }

    #[test]
    fn should_reject_invalid_tag() {
        assert!(Locale::parse("").is_err());
        assert!(Locale::parse("!!").is_err());
    }

    #[test]
    fn should_extract_primary_language() {
        assert_eq!(Locale::parse("zh-CN").unwrap().language(), "zh");
        assert_eq!(Locale::parse("en").unwrap().language(), "en");
    }

    #[test]
    fn should_match_same_language_with_missing_region() {
        let zh_cn = Locale::parse("zh-CN").unwrap();
        let zh = Locale::parse("zh").unwrap();
        let en = Locale::parse("en").unwrap();

        assert!(zh_cn.matches(&zh), "zh 应匹配 zh-CN");
        assert!(zh.matches(&zh_cn), "对称");
        assert!(!zh_cn.matches(&en), "不同语言不匹配");
    }

    #[test]
    fn should_not_match_different_regions() {
        let zh_cn = Locale::parse("zh-CN").unwrap();
        let zh_tw = Locale::parse("zh-TW").unwrap();
        assert!(!zh_cn.matches(&zh_tw), "地区不同且都非空 → 不匹配");
    }
}
