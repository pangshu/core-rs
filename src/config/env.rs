//! 运行环境（`APP_ENV` 环境变量），决定加载哪份 `config/{env}.toml`。

use serde::{Deserialize, Serialize}; // 引入 serde 的序列化/反序列化派生宏
use std::fmt; // 引入格式化相关 trait，用于实现 Display

/// 四环境（文档 01 约定）：development / testing / staging / production
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)] // 派生调试/克隆/拷贝/比较/哈希与 serde 能力
#[serde(rename_all = "lowercase")] // 序列化时枚举变体名统一转小写
pub enum Environment { // 定义运行环境枚举
    Development, // 开发环境
    Testing, // 测试环境
    Staging, // 预发布环境
    Production, // 生产环境
}

impl Environment { // 为运行环境实现相关方法
    /// 从 `APP_ENV` 读取；缺省或无法识别时回落 development
    pub fn from_env() -> Self { // 从环境变量构造运行环境
        Self::parse(&std::env::var("APP_ENV").unwrap_or_default()).unwrap_or(Self::Development) // 读取 APP_ENV 并解析，失败则回落开发环境
    }

    pub fn parse(s: &str) -> Option<Self> { // 解析环境字符串为枚举，支持多种别名
        match s.trim().to_ascii_lowercase().as_str() { // 去空白并转小写后匹配
            "dev" | "development" => Some(Self::Development), // dev/development 映射到开发环境
            "test" | "testing" => Some(Self::Testing), // test/testing 映射到测试环境
            "stage" | "staging" => Some(Self::Staging), // stage/staging 映射到预发布环境
            "prod" | "production" => Some(Self::Production), // prod/production 映射到生产环境
            _ => None, // 无法识别的取值返回 None
        }
    }

    /// toml 文件名：`config/{env}.toml`
    pub fn file_stem(&self) -> &'static str { // 返回该环境对应的配置文件名干
        match self { // 按环境枚举返回文件名
            Self::Development => "development", // 开发环境文件名干
            Self::Testing => "testing", // 测试环境文件名干
            Self::Staging => "staging", // 预发布环境文件名干
            Self::Production => "production", // 生产环境文件名干
        }
    }

    pub fn is_prod(&self) -> bool { // 判断是否为生产环境
        matches!(self, Self::Production) // 仅生产环境返回 true
    }
}

impl fmt::Display for Environment { // 为运行环境实现 Display，便于日志输出
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { // 实现格式化方法
        f.write_str(self.file_stem()) // 直接输出文件名干作为展示名
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
