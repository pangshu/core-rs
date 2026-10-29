//! 检索运行期约束（由 `[search]` 配置派生，构造后不可变）。
//!
//! 单独抽出来是因为 `SearchIndex` 也要用（`limit` 截断、查询串预检、严格映射），
//! 而它不该知道 `SearchSection` 的全部字段。

use crate::config::SearchSection;
use crate::config::sections::search::Analyzer;

/// 索引/查询的硬约束。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SearchOptions {
    /// 默认返回条数。
    pub default_limit: usize,
    /// 单次查询返回条数上限。
    pub max_limit: usize,
    /// 查询串最大字符数。
    pub query_max_len: usize,
    /// 查询串括号最大嵌套层数。
    pub query_max_depth: usize,
    /// 是否允许通配/模糊语法。
    pub allow_wildcard: bool,
    /// 严格字段映射。
    pub strict_mapping: bool,
    /// writer 缓冲（字节）。
    pub writer_memory: usize,
    /// 是否启用 jieba 分词。
    pub cjk: bool,
    /// 索引缓存条目上限。
    pub cache_max_indexes: usize,
    /// 每个索引保留的版本数。
    pub keep_versions: usize,
}

impl SearchOptions {
    pub(crate) fn from_config(config: &SearchSection) -> Self {
        Self {
            default_limit: config.default_limit,
            max_limit: config.max_limit,
            query_max_len: config.query_max_len,
            query_max_depth: config.query_max_depth,
            allow_wildcard: config.allow_wildcard,
            strict_mapping: config.strict_mapping,
            writer_memory: config.writer_memory_bytes(),
            cjk: config.analyzer == Analyzer::Cjk,
            cache_max_indexes: config.cache_max_indexes,
            keep_versions: config.keep_versions,
        }
    }

    /// 把调用方给的 `limit` 归一到 `[1, max_limit]`。
    ///
    /// `0` 表示「用默认值」；超过上限**截断**而不是报错（调用方多为分页参数）。
    pub(crate) fn resolve_limit(&self, limit: usize) -> usize {
        let limit = if limit == 0 {
            self.default_limit
        } else {
            limit
        };
        limit.clamp(1, self.max_limit.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SearchOptions {
        SearchOptions::from_config(&SearchSection::default())
    }

    #[test]
    fn should_map_config_fields() {
        let o = options();
        assert_eq!(o.default_limit, 10);
        assert_eq!(o.max_limit, 100);
        assert_eq!(o.writer_memory, 50_000_000);
        assert!(o.cjk);
        assert!(o.strict_mapping);
    }

    #[test]
    fn should_clamp_limit() {
        let o = options();
        assert_eq!(o.resolve_limit(0), 10, "0 = 用默认值");
        assert_eq!(o.resolve_limit(5), 5);
        assert_eq!(o.resolve_limit(100), 100);
        assert_eq!(o.resolve_limit(1_000_000), 100, "超过上限必须截断");
    }
}
