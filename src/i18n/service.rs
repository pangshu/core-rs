//! 国际化服务：加载 `.ftl` 资源、协商语言、按 key 翻译与格式化。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use fluent::FluentArgs;

use crate::config::I18nSection;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::i18n::format::{format_currency, format_date, format_datetime};
use crate::i18n::locale::Locale;
use crate::i18n::negotiate::negotiate;
use crate::i18n::translator::Translator;

thread_local! {
    /// 线程本地翻译器缓存：`"{resource_dir}|{locale}" → Translator`。
    ///
    /// `FluentBundle` 非 `Send`，只能用线程本地缓存；键含资源目录，避免换配置后串味。
    static BUNDLES: RefCell<HashMap<String, Rc<Translator>>> = RefCell::new(HashMap::new());
}

/// 国际化服务（`AppState.i18n`）。
#[derive(Clone)]
pub struct I18n {
    config: I18nSection,
    default: Locale,
    fallback: Locale,
    supported: Vec<Locale>,
    timezone: Tz,
    /// 启动期读入的 FTL 原文：`locale → 内容`（`None` 表示该语言无资源）。
    sources: Arc<HashMap<String, Option<Arc<str>>>>,
}

impl std::fmt::Debug for I18n {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("I18n")
            .field("default", &self.default)
            .field("fallback", &self.fallback)
            .field("supported", &self.supported)
            .field("timezone", &self.timezone.name())
            .finish_non_exhaustive()
    }
}

impl I18n {
    /// 按 `[i18n]` 配置装配（扫描资源目录、读入 FTL 原文）。
    ///
    /// # Errors
    /// 语言标识或时区非法时返回 `50005`（启动期暴露）。
    pub fn from_config(config: &I18nSection) -> AppResult<Self> {
        let default = parse_locale("i18n.default_locale", &config.default_locale)?;
        let fallback = parse_locale("i18n.fallback_locale", &config.fallback_locale)?;
        let timezone = parse_timezone(&config.timezone)?;

        let dir = PathBuf::from(&config.resource_dir);
        let mut sources: HashMap<String, Option<Arc<str>>> = HashMap::new();
        let mut supported = vec![default.clone()];

        for locale in discover_locales(&dir) {
            let source = read_source(&dir, &locale);
            sources.insert(locale.as_str().to_string(), source);
            if !supported.iter().any(|existing| existing == &locale) {
                supported.push(locale);
            }
        }
        if !supported.iter().any(|existing| existing == &fallback) {
            supported.push(fallback.clone());
        }
        // 默认语言即使没有资源也登记，便于 `t()` 明确回退为 key。
        sources
            .entry(default.as_str().to_string())
            .or_insert_with(|| read_source(&dir, &default));

        Ok(Self {
            config: config.clone(),
            default,
            fallback,
            supported,
            timezone,
            sources: Arc::new(sources),
        })
    }

    /// 默认语言。
    #[must_use]
    pub fn default_locale(&self) -> &Locale {
        &self.default
    }

    /// 兜底语言。
    #[must_use]
    pub fn fallback_locale(&self) -> &Locale {
        &self.fallback
    }

    /// 受支持语言（默认语言在前，其余按资源文件名排序）。
    #[must_use]
    pub fn supported(&self) -> &[Locale] {
        &self.supported
    }

    /// 展示时区（`[i18n].timezone`）。
    #[must_use]
    pub fn timezone(&self) -> Tz {
        self.timezone
    }

    /// 默认货币（`[i18n].currency`）。
    #[must_use]
    pub fn currency(&self) -> &str {
        &self.config.currency
    }

    /// 协商语言（`Accept-Language`）。
    #[must_use]
    pub fn negotiate(&self, accept_language: Option<&str>) -> Locale {
        negotiate(accept_language, &self.supported, &self.default)
    }

    /// 借用某语言的翻译器执行一段逻辑（**线程本地缓存**，不重复解析 FTL）。
    ///
    /// ⚠️ `f` 内**不要**再次调用 `with_translator` / `t`（会重复借用线程本地缓存）。
    pub fn with_translator<R>(&self, locale: &Locale, f: impl FnOnce(&Translator) -> R) -> R {
        let key = format!("{}|{}", self.config.resource_dir, locale.as_str());
        let translator = BUNDLES.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache
                .entry(key)
                .or_insert_with(|| Rc::new(self.build_translator(locale)))
                .clone()
        });
        f(&translator)
    }

    /// 取翻译（无参数）。
    #[must_use]
    pub fn t(&self, locale: &Locale, key: &str) -> String {
        self.with_translator(locale, |translator| translator.t(key))
    }

    /// 取翻译（带参数）。
    #[must_use]
    pub fn t_args(&self, locale: &Locale, key: &str, args: &FluentArgs<'_>) -> String {
        self.with_translator(locale, |translator| translator.t_args(key, args))
    }

    /// 按配置时区格式化时间。
    #[must_use]
    pub fn datetime(&self, dt: DateTime<Utc>) -> String {
        format_datetime(dt, self.timezone)
    }

    /// 按配置时区格式化日期。
    #[must_use]
    pub fn date(&self, dt: DateTime<Utc>) -> String {
        format_date(dt, self.timezone)
    }

    /// 按配置货币格式化金额。
    #[must_use]
    pub fn money(&self, amount: f64) -> String {
        format_currency(amount, &self.config.currency)
    }

    /// 构建某语言的翻译器（启动期已读入原文，缺失则返回空翻译器）。
    fn build_translator(&self, locale: &Locale) -> Translator {
        if let Some(Some(source)) = self.sources.get(locale.as_str()) {
            match Translator::from_source(locale.clone(), source) {
                Ok(translator) => return translator,
                Err(error) => {
                    tracing::warn!(%error, locale = %locale, "语言资源解析失败，回退为 key");
                }
            }
        }
        Translator::empty(locale.clone())
    }
}

fn parse_locale(key: &str, raw: &str) -> AppResult<Locale> {
    Locale::parse(raw).map_err(|_| {
        AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("{key} = \"{raw}\" 不是合法的语言标识"),
        )
    })
}

/// 解析 IANA 时区。
fn parse_timezone(raw: &str) -> AppResult<Tz> {
    raw.parse::<Tz>().map_err(|_| {
        AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("i18n.timezone = \"{raw}\" 不是合法的 IANA 时区"),
        )
    })
}

/// 读入某语言的 FTL 原文（`{locale}.ftl` → `{lang}.ftl`）。
fn read_source(dir: &Path, locale: &Locale) -> Option<Arc<str>> {
    for candidate in [
        dir.join(format!("{locale}.ftl")),
        dir.join(format!("{}.ftl", locale.language())),
    ] {
        if candidate.is_file() {
            match std::fs::read_to_string(&candidate) {
                Ok(source) => return Some(Arc::from(source.as_str())),
                Err(error) => {
                    tracing::warn!(%error, path = %candidate.display(), "读取语言资源失败");
                }
            }
        }
    }
    tracing::debug!(locale = %locale, dir = %dir.display(), "未找到语言资源，翻译将回退为 key");
    None
}

/// 扫描资源目录，把 `*.ftl` 的文件名当作受支持语言。
fn discover_locales(dir: &Path) -> Vec<Locale> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut locales: Vec<Locale> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ftl"))
        .filter_map(|entry| {
            let stem = entry.path().file_stem()?.to_str()?.to_string();
            Locale::parse(&stem).ok()
        })
        .collect();

    locales.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    locales.dedup();
    locales
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> I18nSection {
        I18nSection {
            resource_dir: dir.display().to_string(),
            ..I18nSection::default()
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aurora-i18n-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn should_build_with_defaults_when_dir_missing() {
        let service = I18n::from_config(&config(Path::new("no/such/dir"))).unwrap();
        assert_eq!(service.default_locale().as_str(), "zh-CN");
        assert_eq!(service.fallback_locale().as_str(), "en");
        assert_eq!(service.timezone().name(), "UTC");
        assert_eq!(service.currency(), "CNY");
    }

    #[test]
    fn should_discover_supported_locales_from_resource_dir() {
        let dir = temp_dir();
        std::fs::write(dir.join("en-US.ftl"), "hi = Hi\n").unwrap();
        std::fs::write(dir.join("ja.ftl"), "hi = こんにちは\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let service = I18n::from_config(&config(&dir)).unwrap();
        let names: Vec<&str> = service.supported().iter().map(Locale::as_str).collect();

        assert_eq!(names[0], "zh-CN", "默认语言应排在最前");
        assert!(names.contains(&"en-US"));
        assert!(names.contains(&"ja"));
        assert!(!names.contains(&"notes"));
    }

    #[test]
    fn should_translate_discovered_locale() {
        let dir = temp_dir();
        std::fs::write(dir.join("ja.ftl"), "hi = こんにちは\n").unwrap();

        let service = I18n::from_config(&config(&dir)).unwrap();
        let ja = Locale::parse("ja").unwrap();
        assert_eq!(service.t(&ja, "hi"), "こんにちは");
        assert_eq!(service.t(&ja, "missing"), "missing", "漏翻回退为 key");
    }

    #[test]
    fn should_translate_with_args() {
        let dir = temp_dir();
        std::fs::write(dir.join("en.ftl"), "welcome = Welcome, { $name }!\n").unwrap();

        let service = I18n::from_config(&config(&dir)).unwrap();
        let en = Locale::parse("en").unwrap();

        let mut args = FluentArgs::new();
        args.set("name", "Alice");
        assert_eq!(service.t_args(&en, "welcome", &args), "Welcome, Alice!");
    }

    #[test]
    fn should_fall_back_to_language_level_file() {
        let dir = temp_dir();
        // 只有 `zh.ftl`，请求 `zh-CN` 时应命中语言级文件
        std::fs::write(dir.join("zh.ftl"), "hi = 你好\n").unwrap();

        let service = I18n::from_config(&config(&dir)).unwrap();
        assert_eq!(service.t(&Locale::parse("zh-CN").unwrap(), "hi"), "你好");
    }

    #[test]
    fn should_reuse_thread_local_bundle() {
        let dir = temp_dir();
        std::fs::write(dir.join("en.ftl"), "hi = Hi\n").unwrap();

        let service = I18n::from_config(&config(&dir)).unwrap();
        let en = Locale::parse("en").unwrap();

        let first = service.with_translator(&en, |translator| translator.t("hi"));
        let second = service.with_translator(&en, |translator| translator.t("hi"));
        assert_eq!(first, "Hi");
        assert_eq!(second, "Hi");
    }

    #[test]
    fn should_negotiate_against_supported() {
        let dir = temp_dir();
        std::fs::write(dir.join("en-US.ftl"), "hi = Hi\n").unwrap();

        let service = I18n::from_config(&config(&dir)).unwrap();
        assert_eq!(
            service.negotiate(Some("en-US,zh-CN;q=0.5")).as_str(),
            "en-US"
        );
        assert_eq!(service.negotiate(None).as_str(), "zh-CN");
    }

    #[test]
    fn should_reject_invalid_timezone() {
        let config = I18nSection {
            timezone: "Mars/Olympus".to_string(),
            ..I18nSection::default()
        };
        assert!(I18n::from_config(&config).is_err());
    }

    #[test]
    fn should_format_with_configured_timezone_and_currency() {
        let config = I18nSection {
            timezone: "Asia/Shanghai".to_string(),
            currency: "USD".to_string(),
            ..I18nSection::default()
        };
        let service = I18n::from_config(&config).unwrap();
        let dt = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 17, 15, 1, 0).unwrap();

        assert_eq!(service.datetime(dt), "2026-10-17 23:01:00 CST");
        assert_eq!(service.money(9.5), "$9.50");
    }
}
