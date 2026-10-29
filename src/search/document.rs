//! 文档抽象与检索结果。

use serde::Serialize;
use serde::de::DeserializeOwned;
use tantivy::schema::Schema;

/// 可被索引的文档。
///
/// 约定：**`schema()` 的字段名必须与 serde 字段名一致**，且需要回读的字段必须
/// 带 `STORED`（否则 `search` 反序列化会缺字段）。
///
/// `[search].strict_mapping = true`（默认）时，这条约定是**强制**的：文档里出现
/// schema 中没有的字段、或值类型对不上，`add()` 直接返回 `10001`（静默丢弃会让
/// 「按字段过滤」的字段消失，文档进而漏进越权结果）。
///
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// struct ArticleDoc { id: u64, title: String, body: String }
///
/// impl SearchDocument for ArticleDoc {
///     fn schema() -> Schema {
///         let mut builder = Schema::builder();
///         builder.add_u64_field("id", STORED);
///         builder.add_text_field("title", TEXT | STORED);
///         builder.add_text_field("body", TEXT | STORED);
///         builder.build()
///     }
/// }
/// ```
pub trait SearchDocument: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// 索引 schema（字段名与 serde 字段一致；需要回读的字段加 `STORED`）。
    fn schema() -> Schema;
}

/// 一条检索命中。
#[derive(Debug, Clone)]
pub struct SearchHit<T> {
    /// 相关度得分（Tantivy BM25）。
    pub score: f32,
    /// 反序列化后的文档。
    pub doc: T,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Doc {
        id: u64,
        title: String,
    }

    impl SearchDocument for Doc {
        fn schema() -> Schema {
            let mut builder = Schema::builder();
            builder.add_u64_field("id", tantivy::schema::STORED);
            builder.add_text_field("title", tantivy::schema::TEXT | tantivy::schema::STORED);
            builder.build()
        }
    }

    #[test]
    fn should_build_schema_with_document_fields() {
        let schema = Doc::schema();
        assert!(schema.get_field("id").is_ok());
        assert!(schema.get_field("title").is_ok());
    }

    #[test]
    fn should_hold_score_and_doc() {
        let hit = SearchHit {
            score: 1.5,
            doc: Doc {
                id: 1,
                title: "hi".to_string(),
            },
        };
        assert!((hit.score - 1.5).abs() < f32::EPSILON);
        assert_eq!(hit.doc.title, "hi");
    }
}
