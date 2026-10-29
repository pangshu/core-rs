//! 日志输出格式。

use serde::Deserialize;

/// 日志输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum LogFormat {
    /// 开发友好的多行格式。
    Pretty,
    /// 结构化单行 JSON（默认，便于采集与检索）。
    #[default]
    Json,
}
