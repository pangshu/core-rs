//! Locale 类型与解析、回退语言链（文档 三·20：`zh-Hant` → `zh` → 默认语言）。

use crate::config::sections::I18nSettings;

/// 语言标签（BCP-47 简化：`language[-region]`，统一小写主语言 / 大写区域）
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Locale(String);

impl Locale {
    pub fn parse(tag: &str, settings: &I18nSettings) -> Self {
        Self(normalize(tag, settings))
    }

    /// 配置的默认语言
    pub fn default_for(settings: &I18nSettings) -> Self {
        Self(normalize(&settings.default_locale, settings))
    }

    /// 主语言回退（`zh-Hant` → `zh`）；无区域段时 None。
    /// Translator 按此逐级查找——文档承诺的回退链即由这里驱动。
    pub fn language_fallback(&self) -> Option<Locale> {
        self.0
            .split_once('-')
            .filter(|(_, region)| !region.is_empty())
            .map(|(main, _)| Locale(main.to_string()))
    }

    /// 语言回退链：`zh-Hant` → [`zh`] → 默认语言（translator 逐级查找用）
    pub fn fallback_chain(&self, settings: &I18nSettings) -> Vec<Locale> {
        let mut chain = vec![self.clone()];
        if let Some(main) = self.language_fallback() {
            chain.push(main);
        }
        let default = Locale::default_for(settings);
        if !chain.contains(&default) {
            chain.push(default);
        }
        chain
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self("zh-CN".to_string())
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 归一化：主语言小写 / 区域大写，且**只保留字母数字与连字符**——
/// locale 会被用作 HashMap key 与目录文件名（`{dir}/{locale}.ftl`），
/// 一旦有调用方拿它拼路径，`.` / `/` / `\` 就是路径穿越口
fn normalize(tag: &str, _settings: &I18nSettings) -> String {
    let tag: String = tag
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let mut parts = tag.split('-');
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    let region = parts.next().map(|r| r.to_ascii_uppercase());
    match region {
        Some(r) if !r.is_empty() => format!("{lang}-{r}"),
        _ => lang,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> I18nSettings {
        I18nSettings {
            enabled: true,
            default_locale: "zh-CN".into(),
            ..Default::default()
        }
    }

    #[test]
    fn normalization_and_chain() {
        let s = settings();
        let l = Locale::parse("ZH-hant", &s);
        assert_eq!(l.as_str(), "zh-HANT"); // 主语言小写 / 区域大写（BCP-47）
        assert_eq!(
            l.fallback_chain(&s),
            vec![l.clone(), Locale("zh".into()), Locale("zh-CN".into())]
        );
        let plain = Locale::parse("en", &s);
        assert_eq!(plain.fallback_chain(&s), vec![plain.clone(), Locale("zh-CN".into())]);
    }

    #[test]
    fn path_traversal_chars_are_stripped() {
        let s = settings();
        assert_eq!(Locale::parse("../../etc", &s).as_str(), "etc");
        assert_eq!(Locale::parse("..\\win", &s).as_str(), "win");
        assert_eq!(Locale::parse("zh;drop", &s).as_str(), "zhdrop");
    }
}
