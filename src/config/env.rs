//! 运行环境（`APP_ENV` 环境变量），决定加载哪份 `config/{env}.toml`。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 四环境（文档 01 约定）：development / testing / staging / production
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Development,
    Testing,
    Staging,
    Production,
}

impl Environment {
    /// 从 `APP_ENV` 读取；缺省或无法识别时回落 development
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("APP_ENV").unwrap_or_default()).unwrap_or(Self::Development)
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" => Some(Self::Development),
            "test" | "testing" => Some(Self::Testing),
            "stage" | "staging" => Some(Self::Staging),
            "prod" | "production" => Some(Self::Production),
            _ => None,
        }
    }

    /// toml 文件名：`config/{env}.toml`
    pub fn file_stem(&self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Testing => "testing",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    pub fn is_prod(&self) -> bool {
        matches!(self, Self::Production)
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.file_stem())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aliases() {
        assert_eq!(Environment::parse("dev"), Some(Environment::Development));
        assert_eq!(Environment::parse("PROD"), Some(Environment::Production));
        assert_eq!(Environment::parse("staging"), Some(Environment::Staging));
        assert_eq!(Environment::parse("nope"), None);
    }
}
