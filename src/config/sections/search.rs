//! `[search]` 配置段（feature `search`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[search]`。

use byte_unit::Byte;
use serde::Deserialize;

use crate::config::sections::de::de_byte;

/// 分词器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Analyzer {
    /// jieba 中文分词（默认）。
    Cjk,
    /// Tantivy 内置分词（按空白/标点切分）。
    Default,
}

/// 检索配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct SearchSection {
    /// 索引落盘根目录。
    pub index_dir: String,
    /// 默认返回条数（`search(_, 0)` 时使用）。
    pub default_limit: usize,
    /// 单次查询返回条数上限（`limit` 超过即截断）。
    ///
    /// 搜索请求的 `limit` 通常来自客户端，无上限时 `TopDocs` 会按它预分配内存。
    pub max_limit: usize,
    /// 查询串最大字符数。
    pub query_max_len: usize,
    /// 查询串括号最大嵌套层数（Tantivy 的解析器是递归下降，无深度上限会栈溢出）。
    pub query_max_depth: usize,
    /// 是否允许 `*` / `~` 通配与模糊查询（会全词典扫描）。
    pub allow_wildcard: bool,
    /// 严格字段映射：文档字段不在 schema、或类型不符时**报错**而不是静默丢弃。
    pub strict_mapping: bool,
    /// 已打开索引的缓存条目上限（LRU；`strong_count == 1` 的条目才可淘汰）。
    pub cache_max_indexes: usize,
    /// 每个索引保留的旧版本目录数（含当前版本；`1` = 只留当前）。
    pub keep_versions: usize,
    /// 写入缓冲内存。
    #[serde(deserialize_with = "de_byte")]
    pub writer_memory: Byte,
    /// 默认分词器。
    pub analyzer: Analyzer,
    /// 默认索引别名（索引名为空时使用）。
    pub alias: String,
}

impl Default for SearchSection {
    fn default() -> Self {
        Self {
            index_dir: "data/search".to_string(),
            default_limit: 10,
            max_limit: 100,
            query_max_len: 512,
            query_max_depth: 16,
            allow_wildcard: true,
            strict_mapping: true,
            cache_max_indexes: 8,
            keep_versions: 2,
            // 与 `de_byte` 解析 `"50MB"` 一致（byte-unit 的 MB = 1e6）
            writer_memory: Byte::from_u64(50 * 1000 * 1000),
            analyzer: Analyzer::Cjk,
            alias: "default".to_string(),
        }
    }
}

impl SearchSection {
    /// 写入缓冲（字节）。
    #[must_use]
    pub fn writer_memory_bytes(&self) -> usize {
        usize::try_from(self.writer_memory.as_u64()).unwrap_or(usize::MAX)
    }

    /// 校验：目录/别名非空，写入缓冲至少 15MB（Tantivy 的下限）。
    ///
    /// # Errors
    /// 参数非法时返回错误信息。
    pub fn validate(&self) -> Result<(), String> {
        if self.index_dir.trim().is_empty() {
            return Err("search.index_dir 不能为空".to_string());
        }
        if self.alias.trim().is_empty() {
            return Err("search.alias 不能为空（索引名为空时会回退到它）".to_string());
        }
        // 别名会作为目录名使用，必须满足索引名约束（见 `search::engine::validate_index_name`）。
        if !crate::search::engine::is_valid_index_name(&self.alias) {
            return Err(format!(
                "search.alias 只能包含字母/数字/下划线/连字符且不超过 64 字节：{}",
                self.alias
            ));
        }
        if self.default_limit == 0 {
            return Err("search.default_limit 必须大于 0".to_string());
        }
        if self.max_limit == 0 {
            return Err("search.max_limit 必须大于 0".to_string());
        }
        if self.default_limit > self.max_limit {
            return Err(format!(
                "search.default_limit({}) 不能大于 search.max_limit({})",
                self.default_limit, self.max_limit
            ));
        }
        if self.query_max_len == 0 || self.query_max_len > 8192 {
            return Err("search.query_max_len 必须在 1..=8192".to_string());
        }
        if self.query_max_depth == 0 || self.query_max_depth > 64 {
            return Err("search.query_max_depth 必须在 1..=64".to_string());
        }
        if self.cache_max_indexes == 0 {
            return Err("search.cache_max_indexes 必须大于 0".to_string());
        }
        if self.keep_versions == 0 {
            return Err("search.keep_versions 必须大于 0（1 = 只保留当前版本）".to_string());
        }
        // Tantivy 要求 writer 内存 >= 15MB，否则创建 writer 失败。
        if self.writer_memory.as_u64() < 15 * 1000 * 1000 {
            return Err("search.writer_memory 至少 15MB（Tantivy 下限）".to_string());
        }
        // 每个索引常驻一份 writer 缓冲，无上限配置会让进程内存随配置值线性膨胀。
        if self.writer_memory.as_u64() > 4 * 1000 * 1000 * 1000 {
            return Err("search.writer_memory 不能超过 4GB".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_documented_values() {
        let s = SearchSection::default();
        assert_eq!(s.index_dir, "data/search");
        assert_eq!(s.default_limit, 10);
        assert_eq!(s.max_limit, 100);
        assert_eq!(s.query_max_len, 512);
        assert_eq!(s.query_max_depth, 16);
        assert!(s.allow_wildcard);
        assert!(s.strict_mapping);
        assert_eq!(s.cache_max_indexes, 8);
        assert_eq!(s.keep_versions, 2);
        assert_eq!(s.writer_memory_bytes(), 50_000_000);
        assert_eq!(s.analyzer, Analyzer::Cjk);
        assert_eq!(s.alias, "default");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_too_small_writer_memory() {
        let s = SearchSection {
            writer_memory: Byte::from_u64(1024 * 1024),
            ..SearchSection::default()
        };
        assert!(s.validate().is_err(), "低于 15MB 应被拒绝");
    }

    #[test]
    fn should_parse_overrides() {
        let json = r#"{ "index_dir": "/tmp/idx", "default_limit": 20, "writer_memory": "100MB", "analyzer": "default" }"#;
        let s: SearchSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.index_dir, "/tmp/idx");
        assert_eq!(s.default_limit, 20);
        assert_eq!(s.writer_memory_bytes(), 100_000_000);
        assert_eq!(s.analyzer, Analyzer::Default);
    }

    #[test]
    fn should_reject_limit_and_alias_misuse() {
        let over = SearchSection {
            default_limit: 500,
            ..SearchSection::default()
        };
        assert!(over.validate().is_err(), "default_limit 不得超过 max_limit");

        let bad_alias = SearchSection {
            alias: "../escape".to_string(),
            ..SearchSection::default()
        };
        assert!(bad_alias.validate().is_err(), "别名会进路径，必须校验");

        let zero_cache = SearchSection {
            cache_max_indexes: 0,
            ..SearchSection::default()
        };
        assert!(zero_cache.validate().is_err());
    }

    #[test]
    fn should_reject_zero_query_limits() {
        let s = SearchSection {
            query_max_len: 0,
            ..SearchSection::default()
        };
        assert!(s.validate().is_err());

        let s = SearchSection {
            query_max_depth: 0,
            ..SearchSection::default()
        };
        assert!(s.validate().is_err());
    }
}
