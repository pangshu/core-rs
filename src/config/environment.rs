//! 运行环境（[`Environment`]）与 `AURORA_*` 环境变量配置源。
//!
//! # 环境是框架类型，但取值由应用层决定
//!
//! 框架只定义「环境有哪些档」以及每档对护栏的意义，**不规定环境从哪来**：
//!
//! | 取值 | 生产护栏 |
//! |---|---|
//! | `development` | 关闭 |
//! | `test` | 关闭 |
//! | `staging` | 关闭（预发按非生产处理） |
//! | `production` | **全部生效** |
//!
//! 为什么环境留在框架：它是生产护栏的**分支条件**（JWT 密钥必填、CORS 禁 `*`、
//! `/metrics` 需令牌……）。如果这个判据下沉到各应用，每个应用都会自己判一遍，
//! 而且总有人为了让服务先起来写成 `Development`。
//!
//! 为什么"环境从哪来"不下沉：那是部署约定。用 `AURORA_ENV` 还是 `MYAPP_PROFILE`、
//! 4 个值还是 3 个值，都是应用层的事——框架提供
//! [`preset::env_from_var`](crate::config::preset::env_from_var) 作为可选约定，
//! 不用也可以自己映射成 [`Environment`] 传进
//! [`ConfigSource::new`](crate::config::ConfigSource::new)。
//!
//! 配置文件里**没有** `[app].env` 这个字段：环境不是"配置项"，而是"选配置的开关"。
//! 放在文件里就会出现两个来源，而两者冲突时无论以谁为准都存在降级路径
//! （生产被降级成开发 → 错误详情 / OpenAPI / metrics / 宽松 CORS 一起打开）。

use std::fmt;
use std::str::FromStr;

/// 框架默认的环境变量前缀（应用可改）。
pub const DEFAULT_ENV_PREFIX: &str = "AURORA";

/// 层级分隔符。
pub(crate) const SEPARATOR: &str = "__";

/// 运行环境。
///
/// 封闭的 4 值集合，见模块文档。默认值是 [`Environment::Production`]
/// （fail-closed）：任何"不知道该按哪档跑"的场合都取最严的一档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Environment {
    /// 开发环境。
    Development,
    /// 测试环境。
    Test,
    /// 预发环境。
    Staging,
    /// 生产环境（默认值）。
    #[default]
    Production,
}

impl Environment {
    /// 全部合法取值（顺序即文档顺序）。
    pub const ALL: [Self; 4] = [
        Self::Development,
        Self::Test,
        Self::Staging,
        Self::Production,
    ];

    /// 规范名（同时是配置文件名的主干）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Test => "test",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    /// 是否生产环境（**所有生产护栏的唯一判据**）。
    #[must_use]
    pub const fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }

    /// 严格解析环境名（大小写不敏感，接受首尾空白）。
    ///
    /// # Errors
    /// 空串或不在 [`Self::ALL`] 内时返回错误说明。**不做别名映射**：
    /// `dev` / `prod` / `uat` 一律拒绝，否则"读哪份配置"就取决于隐式约定。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "development" => Ok(Self::Development),
            "test" => Ok(Self::Test),
            "staging" => Ok(Self::Staging),
            "production" => Ok(Self::Production),
            "" => Err(
                "运行环境为空：请显式给出 development / test / staging / production".to_string(),
            ),
            other => Err(format!(
                "{other:?} 不是合法的运行环境：只支持 development / test / staging / production。\
                 不接受别名（dev / prod / uat / ci 等）——环境决定生产护栏是否生效，\
                 别名会让「按哪一档跑」取决于隐式约定"
            )),
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Environment {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// 构造以指定前缀的环境变量配置源（`{prefix}_{SECTION}__{KEY}`）。
pub(crate) fn env_source(prefix: &str) -> config::Environment {
    config::Environment::with_prefix(prefix)
        .prefix_separator("_")
        .separator(SEPARATOR)
        .try_parsing(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_all_four_environments() {
        for env in Environment::ALL {
            assert_eq!(Environment::parse(env.as_str()), Ok(env));
        }
        // 大小写 / 首尾空白不敏感
        assert_eq!(
            Environment::parse("  PRODUCTION "),
            Ok(Environment::Production)
        );
        assert_eq!(Environment::parse("Staging"), Ok(Environment::Staging));
    }

    /// 封闭集合：别名与自定义名字一律拒绝。
    ///
    /// 旧实现直接把 `AURORA_ENV` 当文件名主干用，`prod` 会去读 `prod.toml`
    /// （不存在则静默回退），`../../etc/passwd` 会读到配置目录之外。
    #[test]
    fn should_reject_aliases_and_path_traversal() {
        for raw in [
            "dev",
            "prod",
            "uat",
            "ci",
            "testing",
            "../production",
            "../../etc/passwd",
        ] {
            assert!(
                Environment::parse(raw).is_err(),
                "{raw:?} 必须被拒绝（环境是封闭集合）"
            );
        }
        assert!(Environment::parse("").is_err(), "空串必须被拒绝");
    }

    #[test]
    fn should_default_to_production() {
        assert_eq!(Environment::default(), Environment::Production);
        assert!(Environment::default().is_production());
    }

    #[test]
    fn should_only_treat_production_as_production() {
        for env in [
            Environment::Development,
            Environment::Test,
            Environment::Staging,
        ] {
            assert!(!env.is_production(), "{env} 不应触发生产护栏");
        }
        assert!(Environment::Production.is_production());
    }

    #[test]
    fn should_display_canonical_name() {
        assert_eq!(Environment::Production.to_string(), "production");
        assert_eq!(Environment::Staging.to_string(), "staging");
        // 规范名即配置文件名主干，必须是小写
        for env in Environment::ALL {
            assert_eq!(env.as_str(), env.as_str().to_ascii_lowercase());
        }
    }
}
