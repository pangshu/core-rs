//! Locale 类型与解析、回退语言链（文档 三·20：`zh-Hant` → `zh` → 默认语言）。

use crate::config::sections::I18nSettings; // 引入 i18n 配置节，供解析与回退使用

/// 语言标签（BCP-47 简化：`language[-region]`，统一小写主语言 / 大写区域）
#[derive(Debug, Clone, PartialEq, Eq, Hash)] // 派生调试/克隆/相等/哈希，使其可作 HashMap key
pub struct Locale(String); // 新建类型包装归一化后的语言标签字符串

impl Locale {
    pub fn parse(tag: &str, settings: &I18nSettings) -> Self { // 从任意标签解析出规范化 Locale
        Self(normalize(tag, settings)) // 归一化后包装返回
    }

    /// 配置的默认语言
    pub fn default_for(settings: &I18nSettings) -> Self { // 取配置中的默认语言
        Self(normalize(&settings.default_locale, settings)) // 归一化默认语言后包装返回
    }

    /// 主语言回退（`zh-Hant` → `zh`）；无区域段时 None。
    /// Translator 按此逐级查找——文档承诺的回退链即由这里驱动。
    pub fn language_fallback(&self) -> Option<Locale> { // 求主语言回退项（去掉区域段）
        self.0 // 从内部标签字符串开始
            .split_once('-') // 在第一个连字符处切成 (主语言, 区域)
            .filter(|(_, region)| !region.is_empty()) // 区域为空则不构成回退
            .map(|(main, _)| Locale(main.to_string())) // 只保留主语言并构造新 Locale
    }

    /// 语言回退链：`zh-Hant` → [`zh`] → 默认语言（translator 逐级查找用）
    pub fn fallback_chain(&self, settings: &I18nSettings) -> Vec<Locale> { // 构造逐级查找用的回退链
        let mut chain = vec![self.clone()]; // 链首为自身
        if let Some(main) = self.language_fallback() { // 若有主语言回退项
            chain.push(main); // 追加主语言
        }
        let default = Locale::default_for(settings); // 取得配置默认语言
        if !chain.contains(&default) { // 默认语言尚未在链中时
            chain.push(default); // 追加默认语言作为兜底
        }
        chain // 返回回退链
    }

    pub fn as_str(&self) -> &str { // 以字符串切片形式暴露内部标签
        &self.0 // 借用内部字符串
    }
}

impl Default for Locale { // 为 Locale 实现 Default
    fn default() -> Self { // 默认值
        Self("zh-CN".to_string()) // 默认语言固定为 zh-CN
    }
}

impl std::fmt::Display for Locale { // 实现 Display 以便日志/展示
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 格式化入口
        f.write_str(&self.0) // 直接写出内部标签
    }
}

/// 归一化：主语言小写 / 区域大写，且**只保留字母数字与连字符**——
/// locale 会被用作 HashMap key 与目录文件名（`{dir}/{locale}.ftl`），
/// 一旦有调用方拿它拼路径，`.` / `/` / `\` 就是路径穿越口
fn normalize(tag: &str, _settings: &I18nSettings) -> String { // 归一化语言标签（配置参数暂未使用）
    let tag: String = tag // 先过滤非法字符
        .trim() // 去掉首尾空白
        .chars() // 逐字符处理
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-') // 只保留字母数字与连字符，杜绝路径穿越
        .collect(); // 收集成新的标签字符串
    let mut parts = tag.split('-'); // 按连字符切出主语言与区域
    let lang = parts.next().unwrap_or("").to_ascii_lowercase(); // 主语言统一小写
    let region = parts.next().map(|r| r.to_ascii_uppercase()); // 区域统一大写
    match region { // 依据是否存在区域决定输出形式
        Some(r) if !r.is_empty() => format!("{lang}-{r}"), // 有区域则输出 lang-REGION
        _ => lang, // 无区域则仅输出主语言
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
