//! 消息目录与查找（Fluent，文档 三·20）：加载 `{dir}/{locale}.ftl`，
//! 支持缺省回退（按 Locale 回退链逐级查找）。
//!
//! FTL 里约定错误消息键为 `error-{code}`，如：
//!
//! ```ftl
//! error-1001 = 订单不存在
//! error-1002 = 库存不足，剩余 { $remain }
//! ```

use std::collections::HashMap;
use std::sync::RwLock;

use fluent::{FluentArgs, FluentBundle, FluentResource, FluentValue};

use crate::config::sections::I18nSettings;
use crate::error::AppResult;
use crate::i18n::locale::Locale;

/// 翻译器：进程内一份，`Arc` 共享（可由应用挂到 CoreState 或自行管理）
pub struct Translator {
    bundles: RwLock<HashMap<Locale, FluentBundle<FluentResource>>>,
    fallbacks: Vec<Locale>,
}

impl Translator {
    /// 从目录加载全部 `{locale}.ftl`（目录不存在时返回空翻译器，
    /// 调用 `translate` 未命中即回退原始消息——i18n 缺失不阻塞启动）
    pub fn load(settings: &I18nSettings) -> AppResult<Self> {
        let mut bundles = HashMap::new();
        let dir = std::path::Path::new(&settings.catalog_dir);
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("ftl") {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let locale = Locale::parse(&stem, settings);
                let source = std::fs::read_to_string(&path)?;
                let resource = FluentResource::try_new(source)
                    .map_err(|(_, errs)| {
                        crate::error::AppError::internal(format!(
                            "fluent parse errors in {}: {errs:?}",
                            path.display()
                        ))
                    })?;
                let mut bundle = FluentBundle::new(vec![fluent_langid(&locale)]);
                bundle
                    .add_resource(resource)
                    .map_err(|errs| {
                        crate::error::AppError::internal(format!(
                            "fluent overlapping ids in {}: {:?}",
                            path.display(),
                            errs
                        ))
                    })?;
                bundles.insert(locale, bundle);
            }
            tracing::info!(dir = %settings.catalog_dir, locales = bundles.len(), "i18n catalogs loaded");
        } else {
            tracing::warn!(dir = %settings.catalog_dir, "i18n catalog dir missing, translations disabled");
        }

        // 回退链：配置的 fallbacks + 默认语言
        let mut fallbacks: Vec<Locale> = settings
            .fallbacks
            .iter()
            .map(|f| Locale::parse(f, settings))
            .collect();
        let default = Locale::default_for(settings);
        if !fallbacks.contains(&default) {
            fallbacks.push(default);
        }

        Ok(Self {
            bundles: RwLock::new(bundles),
            fallbacks,
        })
    }

    /// 翻译：按 locale → 主语言回退（zh-Hant → zh）→ 配置回退链 逐级查找；
    /// 全部未命中返回 None
    pub fn translate(&self, locale: &Locale, key: &str, args: &[(&str, FluentValue<'_>)]) -> Option<String> {
        let bundles = self.bundles.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        // 回退链 = locale 自身 + 主语言回退（Locale::language_fallback）+ 配置的
        // fallbacks（含默认语言）。此前只查 [locale] + fallbacks，文档承诺的
        // zh-Hant → zh 回退从未生效
        let mut chain = vec![locale.clone()];
        if let Some(main) = locale.language_fallback() {
            if !chain.contains(&main) {
                chain.push(main);
            }
        }
        for f in &self.fallbacks {
            if !chain.contains(f) {
                chain.push(f.clone());
            }
        }
        for candidate in chain {
            let Some(bundle) = bundles.get(&candidate) else {
                continue;
            };
            let Some(message) = bundle.get_message(key) else {
                continue;
            };
            let Some(pattern) = message.value() else {
                continue;
            };
            let mut fa = FluentArgs::new();
            for (k, v) in args {
                fa.set(*k, v.clone());
            }
            let mut errors = Vec::new();
            let out = bundle.format_pattern(pattern, Some(&fa), &mut errors);
            if !errors.is_empty() {
                tracing::debug!(key, errors = ?errors, "fluent format errors");
            }
            return Some(out.to_string());
        }
        None
    }

    /// 已加载语言
    pub fn locales(&self) -> Vec<Locale> {
        self.bundles
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

fn fluent_langid(locale: &Locale) -> unic_langid::LanguageIdentifier {
    locale
        .as_str()
        .parse::<unic_langid::LanguageIdentifier>()
        .unwrap_or_else(|_| "en".parse().expect("static langid"))
}
