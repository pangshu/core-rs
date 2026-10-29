//! 查询构造。
//!
//! 薄封装：只承载「查询串 + 允许字段」，真正的解析交给 Tantivy 的
//! `QueryParser`（在 [`SearchIndex::search`](crate::search::SearchIndex::search) 内部完成）。

/// 检索查询。
///
/// `fields` 是**允许字段集合**（不是「未限定字段时的默认字段」）：
/// Tantivy 的 `field:term` 语法会对 schema 里**任意**字段生效，与默认字段无关，
/// 所以查询串里显式写出的字段也必须在集合内，否则 `search` 返回 `10001`。
///
/// ```ignore
/// // 只允许查 title：用户输入 "body:机密" 会被拒绝（而不是悄悄查到 body）
/// let q = Query::parse(user_input, &["title"]);
/// ```
///
/// 长度 / 嵌套深度 / 通配语法在 `search` 时按 `[search]` 配置校验，所以
/// [`Query::parse`] 本身不返回错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    text: String,
    fields: Vec<String>,
}

impl Query {
    /// 在指定字段上检索。
    ///
    /// `fields` 为空等价于「允许索引中所有已建索引的字段」（未限定的词仍然只查
    /// 可检索文本字段）。
    #[must_use]
    pub fn parse(text: impl Into<String>, fields: &[&str]) -> Self {
        Self {
            text: text.into(),
            fields: fields.iter().map(|field| (*field).to_string()).collect(),
        }
    }

    /// 全字段检索。
    #[must_use]
    pub fn all(text: impl Into<String>) -> Self {
        Self::parse(text, &[])
    }

    /// 查询串。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 允许字段（空表示全字段）。
    #[must_use]
    pub fn fields(&self) -> &[String] {
        &self.fields
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_carry_text_and_fields() {
        let query = Query::parse("世界", &["title", "body"]);
        assert_eq!(query.text(), "世界");
        assert_eq!(query.fields(), ["title", "body"]);
    }

    #[test]
    fn should_default_to_all_fields() {
        let query = Query::all("hello");
        assert!(query.fields().is_empty());
    }
}
