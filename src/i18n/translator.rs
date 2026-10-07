//! 消息目录与查找（Fluent，文档 三·20）：加载 `{dir}/{locale}.ftl`，
//! 支持缺省回退（按 Locale 回退链逐级查找）。
//!
//! FTL 里约定错误消息键为 `error-{code}`，如：
//!
//! ```ftl
//! error-1001 = 订单不存在
//! error-1002 = 库存不足，剩余 { $remain }
//! ```

use std::collections::HashMap; // 引入 HashMap，按 Locale 存放已加载的消息束
use std::sync::RwLock; // 引入读写锁，保护消息束表支持并发读

use fluent::{FluentArgs, FluentBundle, FluentResource, FluentValue}; // 引入 Fluent 的消息资源/束/参数/值类型

use crate::config::sections::I18nSettings; // 引入 i18n 配置节
use crate::error::AppResult; // 引入统一结果类型，加载失败时上抛
use crate::i18n::locale::Locale; // 引入 Locale 类型作为消息束的键

/// 翻译器：进程内一份，`Arc` 共享（可由应用挂到 CoreState 或自行管理）
pub struct Translator { // 翻译器：持有各语言消息束与回退链
    bundles: RwLock<HashMap<Locale, FluentBundle<FluentResource>>>, // 语言 → Fluent 消息束，读多写少用读写锁
    fallbacks: Vec<Locale>, // 配置回退链（含默认语言），逐级兜底查找
}

impl Translator {
    /// 从目录加载全部 `{locale}.ftl`（目录不存在时返回空翻译器，
    /// 调用 `translate` 未命中即回退原始消息——i18n 缺失不阻塞启动）
    pub fn load(settings: &I18nSettings) -> AppResult<Self> { // 从配置目录加载所有消息束
        let mut bundles = HashMap::new(); // 暂存待加载的消息束表
        let dir = std::path::Path::new(&settings.catalog_dir); // 消息目录路径
        if dir.is_dir() { // 目录存在才扫描
            for entry in std::fs::read_dir(dir)? { // 遍历目录项，IO 失败上抛
                let path = entry?.path(); // 取出该目录项的完整路径
                if path.extension().and_then(|e| e.to_str()) != Some("ftl") { // 只处理 .ftl 文件
                    continue;
                }
                let stem = path // 取文件名主干作为 locale
                    .file_stem() // 去掉扩展名
                    .and_then(|s| s.to_str()) // 转为 &str
                    .unwrap_or_default() // 非法 UTF-8 时用空串兜底
                    .to_string(); // 转为自有 String
                let locale = Locale::parse(&stem, settings); // 归一化文件名为主 Locale
                let source = std::fs::read_to_string(&path)?; // 读取 FTL 文件内容，失败上抛
                let resource = FluentResource::try_new(source) // 解析 FTL 为资源
                    .map_err(|(_, errs)| { // 解析出错时转换为内部错误
                        crate::error::AppError::internal(format!( // 构造内部错误并带上错误详情
                            "fluent parse errors in {}: {errs:?}", // 错误消息模板（含路径与错误）
                            path.display() // 出错文件路径
                        ))
                    })?;
                let mut bundle = FluentBundle::new(vec![fluent_langid(&locale)]); // 为该 Locale 新建消息束
                bundle // 向消息束加入资源
                    .add_resource(resource) // 装载资源
                    .map_err(|errs| { // id 冲突等错误转换为内部错误
                        crate::error::AppError::internal(format!( // 构造内部错误并带上冲突详情
                            "fluent overlapping ids in {}: {:?}", // 错误消息模板（含路径与冲突）
                            path.display(), // 出错文件路径
                            errs // 冲突的错误列表
                        ))
                    })?;
                bundles.insert(locale, bundle); // 登记该语言的完整消息束
            }
            tracing::info!(dir = %settings.catalog_dir, locales = bundles.len(), "i18n catalogs loaded"); // 记录加载成功的语言数量
        } else {
            tracing::warn!(dir = %settings.catalog_dir, "i18n catalog dir missing, translations disabled"); // 目录缺失仅告警，不阻塞启动
        }

        // 回退链：配置的 fallbacks + 默认语言
        let mut fallbacks: Vec<Locale> = settings // 由配置的 fallbacks 构造回退链
            .fallbacks // 配置中的回退语言列表
            .iter() // 逐项遍历
            .map(|f| Locale::parse(f, settings)) // 归一化为 Locale
            .collect();
        let default = Locale::default_for(settings); // 取得默认语言
        if !fallbacks.contains(&default) { // 默认语言不在链中时
            fallbacks.push(default); // 追加默认语言作为最终兜底
        }

        Ok(Self { // 组装翻译器
            bundles: RwLock::new(bundles), // 消息束表加读写锁
            fallbacks, // 保存回退链
        })
    }

    /// 翻译：按 locale → 主语言回退（zh-Hant → zh）→ 配置回退链 逐级查找；
    /// 全部未命中返回 None
    pub fn translate(&self, locale: &Locale, key: &str, args: &[(&str, FluentValue<'_>)]) -> Option<String> { // 按回退链查找并格式化消息
        let bundles = self.bundles.read().unwrap_or_else(std::sync::PoisonError::into_inner); // 取读锁，锁毒化时取出内部值
        // 回退链 = locale 自身 + 主语言回退（Locale::language_fallback）+ 配置的
        // fallbacks（含默认语言）。此前只查 [locale] + fallbacks，文档承诺的
        // zh-Hant → zh 回退从未生效
        let mut chain = vec![locale.clone()]; // 回退链以目标语言起始
        if let Some(main) = locale.language_fallback() { // 存在主语言回退项时
            if !chain.contains(&main) { // 且尚未在链中
                chain.push(main); // 追加主语言
            }
        }
        for f in &self.fallbacks { // 继续追加配置回退链
            if !chain.contains(f) { // 避免重复
                chain.push(f.clone()); // 追加该回退语言
            }
        }
        for candidate in chain { // 沿回退链逐级尝试
            let Some(bundle) = bundles.get(&candidate) else { // 该语言无消息束则跳过
                continue;
            };
            let Some(message) = bundle.get_message(key) else { // 该语言无此消息键则跳过
                continue;
            };
            let Some(pattern) = message.value() else { // 消息无默认值则跳过
                continue;
            };
            let mut fa = FluentArgs::new(); // 构造格式化参数容器
            for (k, v) in args { // 逐个写入调用方传入的参数
                fa.set(*k, v.clone()); // 设置键值对
            }
            let mut errors = Vec::new(); // 收集格式化过程中的错误
            let out = bundle.format_pattern(pattern, Some(&fa), &mut errors); // 渲染消息模板
            if !errors.is_empty() { // 渲染有错误时
                tracing::debug!(key, errors = ?errors, "fluent format errors"); // 记录调试日志但不中断
            }
            return Some(out.to_string()); // 返回首个命中的翻译结果
        }
        None // 全链未命中
    }

    /// 已加载语言
    pub fn locales(&self) -> Vec<Locale> { // 列出已加载的语言
        self.bundles // 从消息束表开始
            .read() // 取读锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // 锁毒化时取出内部值
            .keys() // 取全部键（语言）
            .cloned() // 克隆为自有 Locale
            .collect() // 收集为 Vec 返回
    }
}

fn fluent_langid(locale: &Locale) -> unic_langid::LanguageIdentifier { // 把 Locale 转为 Fluent 需要的 langid
    locale // 从 Locale 开始
        .as_str() // 取标签字符串
        .parse::<unic_langid::LanguageIdentifier>() // 解析为 LanguageIdentifier
        .unwrap_or_else(|_| "en".parse().expect("static langid")) // 解析失败回落到 en（静态串必然可解析）
}
