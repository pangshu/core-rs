//! Fluent 翻译器（`fluent`）。
//!
//! 一个 [`Translator`] 对应**一个语言**的 `.ftl` 资源；缺失的 key 原样返回 key
//! （便于开发期发现漏翻，且不会让接口报错）。

use std::path::Path;

use fluent::{FluentArgs, FluentBundle, FluentResource};
use unic_langid::LanguageIdentifier;

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::i18n::locale::Locale;

/// 单语言翻译器。
pub struct Translator {
    locale: Locale,
    bundle: FluentBundle<FluentResource>,
}

impl std::fmt::Debug for Translator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Translator")
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

impl Translator {
    /// 空翻译器（无资源；`t` 一律回退为 key）。
    #[must_use]
    pub fn empty(locale: Locale) -> Self {
        Self {
            locale: locale.clone(),
            bundle: new_bundle(&locale),
        }
    }

    /// 从 FTL 原文构建（[`I18n`](crate::i18n::I18n) 启动期已读入原文，走这条路径）。
    ///
    /// # Errors
    /// FTL 语法错误 → `50005`。
    pub fn from_source(locale: Locale, source: &str) -> AppResult<Self> {
        let resource = FluentResource::try_new(source.to_string()).map_err(|(_, errors)| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("解析语言资源失败（{}）: {errors:?}", locale.as_str()),
            )
        })?;

        let mut bundle = new_bundle(&locale);
        bundle.add_resource(resource).map_err(|errors| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("装载语言资源失败（{}）: {errors:?}", locale.as_str()),
            )
        })?;

        Ok(Self { locale, bundle })
    }

    /// 从 `.ftl` 文件加载。
    ///
    /// # Errors
    /// 文件读取失败 → `50005`；FTL 语法错误 → `50005`（启动期即暴露）。
    pub fn load(locale: Locale, path: &Path) -> AppResult<Self> {
        let source = std::fs::read_to_string(path).map_err(|error| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("读取语言资源失败 {}: {error}", path.display()),
            )
        })?;
        Self::from_source(locale, &source)
    }

    /// 所属语言。
    #[must_use]
    pub fn locale(&self) -> &Locale {
        &self.locale
    }

    /// 是否含某条消息。
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.bundle.get_message(key).is_some()
    }

    /// 取翻译（无参数）。
    #[must_use]
    pub fn t(&self, key: &str) -> String {
        self.t_args(key, &FluentArgs::default())
    }

    /// 取翻译（带参数）。
    #[must_use]
    pub fn t_args(&self, key: &str, args: &FluentArgs<'_>) -> String {
        let Some(pattern) = self
            .bundle
            .get_message(key)
            .and_then(|message| message.value())
        else {
            // 漏翻：返回 key 本身，便于开发期发现。
            return key.to_string();
        };

        let mut errors = Vec::new();
        let formatted = self.bundle.format_pattern(pattern, Some(args), &mut errors);
        if !errors.is_empty() {
            tracing::debug!(key, ?errors, "Fluent 格式化告警（已降级输出）");
        }
        formatted.to_string()
    }
}

/// 新建 bundle，并**关闭双向文本隔离标记**。
///
/// Fluent 默认会把插值包在 `U+2068` / `U+2069`（FSI/PDI）里以保证 RTL 正确性，
/// 但这些是不可见字符，会混进 JSON 响应、让字符串比较与前端展示都出问题。
/// 框架默认关闭；确有 RTL 需求可自行构造 [`Translator`] 并开启。
fn new_bundle(locale: &Locale) -> FluentBundle<FluentResource> {
    let mut bundle = FluentBundle::new(vec![langid(locale)]);
    bundle.set_use_isolating(false);
    bundle
}

/// 把 [`Locale`] 转为 Fluent 需要的 `LanguageIdentifier`。
fn langid(locale: &Locale) -> LanguageIdentifier {
    locale
        .as_str()
        .parse()
        .unwrap_or_else(|_| LanguageIdentifier::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ftl(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aurora-i18n-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn should_translate_plain_message() {
        let path = write_ftl("zh-CN.ftl", "greeting = 你好\n");
        let translator = Translator::load(Locale::parse("zh-CN").unwrap(), &path).unwrap();
        assert_eq!(translator.t("greeting"), "你好");
    }

    #[test]
    fn should_translate_with_args() {
        let path = write_ftl("zh-CN.ftl", "welcome = 欢迎，{ $name }！\n");
        let translator = Translator::load(Locale::parse("zh-CN").unwrap(), &path).unwrap();

        let mut args = FluentArgs::new();
        args.set("name", "小明");
        assert_eq!(translator.t_args("welcome", &args), "欢迎，小明！");
    }

    #[test]
    fn should_fall_back_to_key_when_missing() {
        let translator = Translator::empty(Locale::parse("en").unwrap());
        assert_eq!(translator.t("missing.key"), "missing.key");
        assert!(!translator.has("missing.key"));
    }

    #[test]
    fn should_reject_invalid_ftl() {
        let path = write_ftl("bad.ftl", "= broken syntax =\n");
        assert!(Translator::load(Locale::parse("en").unwrap(), &path).is_err());
    }

    #[test]
    fn should_reject_missing_file() {
        let path = std::path::Path::new("definitely/not/here.ftl");
        assert!(Translator::load(Locale::parse("en").unwrap(), path).is_err());
    }
}
