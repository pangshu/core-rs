//! schema 与文档转换、中文分词注册。

use std::sync::{Arc, OnceLock};

use jieba_rs::Jieba;
use serde::Serialize;
use serde_json::{Map, Value as JsonValue};
use tantivy::Index;
use tantivy::schema::{Field, FieldType, Schema, TantivyDocument, Value as _};
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, Token, TokenStream, Tokenizer};

use crate::errors::{AppError, AppResult, ErrorCode};

/// jieba 分词器名（注册后同时**覆盖内置 `default`**，使 `TEXT` 字段自动生效）。
pub const JIEBA_TOKENIZER: &str = "aurora_jieba";

/// 进程内共享的 jieba 词典。
///
/// 词典本身是只读的，每个索引各建一份纯属浪费（一份约数十 MB）；索引缓存又有条目
/// 上限，共享后内存不再随「打开过的索引数」增长。
fn shared_jieba() -> Arc<Jieba> {
    static JIEBA: OnceLock<Arc<Jieba>> = OnceLock::new();
    JIEBA.get_or_init(|| Arc::new(Jieba::new())).clone()
}

/// 把 jieba 分词器注册进索引，并覆盖 `default`。
///
/// 覆盖 `default` 的原因：用户 schema 里的 `TEXT` 等价于「tokenizer = default」，
/// 覆盖后无需让业务感知分词器名即可获得中文分词（见 `docs/07` §3.16）。
pub(crate) fn register_cjk(index: &Index) {
    let analyzer = TextAnalyzer::builder(JiebaTokenizer {
        jieba: shared_jieba(),
    })
    .filter(LowerCaser)
    .build();

    index
        .tokenizers()
        .register(JIEBA_TOKENIZER, analyzer.clone());
    index.tokenizers().register("default", analyzer);
}

/// 基于 jieba 的 Tokenizer。
#[derive(Clone)]
struct JiebaTokenizer {
    jieba: Arc<Jieba>,
}

impl Tokenizer for JiebaTokenizer {
    type TokenStream<'a> = JiebaTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let base = text.as_ptr() as usize;
        let tokens = self
            .jieba
            .cut(text, true)
            .into_iter()
            .filter(|piece| !piece.trim().is_empty())
            .enumerate()
            .map(|(position, piece)| {
                let offset_from = piece.as_ptr() as usize - base;
                Token {
                    offset_from,
                    offset_to: offset_from + piece.len(),
                    position,
                    text: piece.to_string(),
                    position_length: 1,
                }
            })
            .collect::<Vec<_>>();

        JiebaTokenStream {
            tokens: tokens.into_iter(),
            current: Token::default(),
        }
    }
}

struct JiebaTokenStream {
    tokens: std::vec::IntoIter<Token>,
    current: Token,
}

impl TokenStream for JiebaTokenStream {
    fn advance(&mut self) -> bool {
        match self.tokens.next() {
            Some(token) => {
                self.current = token;
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

/// 文档 → Tantivy 文档（按 schema 字段名映射）。
///
/// `strict = true` 时**不再静默丢数据**：文档字段不在 schema（多半是拼写错误）、
/// 值类型不符、或 schema 字段类型不支持自动映射，都返回 `10001`。静默丢弃会让
/// 「按字段过滤」的那类文档（租户 / 可见性 / 状态）缺字段，进而漏进越权结果。
///
/// `strict = false` 保持旧行为（忽略多余字段与类型不符的值）。
pub(crate) fn to_tantivy<T: Serialize>(
    schema: &Schema,
    document: &T,
    strict: bool,
) -> AppResult<TantivyDocument> {
    let json = serde_json::to_value(document).map_err(|error| {
        AppError::biz(
            ErrorCode::INVALID_PARAM,
            format!("待索引文档序列化失败：{error}"),
        )
    })?;

    let JsonValue::Object(object) = json else {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "待索引文档必须是 JSON 对象",
        ));
    };

    if strict {
        for key in object.keys() {
            if schema.find_field(key).is_none() {
                return Err(AppError::biz(
                    ErrorCode::INVALID_PARAM,
                    format!("文档字段 {key} 不在索引 schema 中（检查字段名拼写）"),
                ));
            }
        }
    }

    let mut out = TantivyDocument::default();
    for (field, entry) in schema.fields() {
        let Some(value) = object.get(entry.name()) else {
            continue;
        };
        add_value(
            &mut out,
            field,
            entry.name(),
            entry.field_type(),
            value,
            strict,
        )?;
    }
    Ok(out)
}

fn add_value(
    document: &mut TantivyDocument,
    field: Field,
    name: &str,
    field_type: &FieldType,
    value: &JsonValue,
    strict: bool,
) -> AppResult<()> {
    match value {
        // 数组 → 同名字段多值
        JsonValue::Array(items) => {
            for item in items {
                add_value(document, field, name, field_type, item, strict)?;
            }
            return Ok(());
        }
        JsonValue::Null => return Ok(()),
        _ => {}
    }

    let added = match field_type {
        FieldType::Str(_) => value
            .as_str()
            .map(|text| document.add_text(field, text))
            .is_some(),
        FieldType::U64(_) => value
            .as_u64()
            .map(|number| document.add_u64(field, number))
            .is_some(),
        FieldType::I64(_) => value
            .as_i64()
            .map(|number| document.add_i64(field, number))
            .is_some(),
        FieldType::F64(_) => value
            .as_f64()
            .map(|number| document.add_f64(field, number))
            .is_some(),
        FieldType::Bool(_) => value
            .as_bool()
            .map(|flag| document.add_bool(field, flag))
            .is_some(),
        // 其余类型（Date / Facet / Bytes / IpAddr / JsonObject）不自动映射，
        // 需要时请自行构造 TantivyDocument 后用 `SearchIndex::add_document`。
        _ => false,
    };

    if added || !strict {
        return Ok(());
    }

    let expected = field_type_label(field_type);
    let message = if matches!(
        field_type,
        FieldType::Date(_)
            | FieldType::Facet(_)
            | FieldType::Bytes(_)
            | FieldType::IpAddr(_)
            | FieldType::JsonObject(_)
    ) {
        format!("字段 {name}（{expected}）不支持自动映射，请用 add_document 构造")
    } else {
        format!("字段 {name} 的值类型与 schema 不符（期望 {expected}）")
    };
    Err(AppError::biz(ErrorCode::INVALID_PARAM, message))
}

fn field_type_label(field_type: &FieldType) -> &'static str {
    match field_type {
        FieldType::Str(_) => "text",
        FieldType::U64(_) => "u64",
        FieldType::I64(_) => "i64",
        FieldType::F64(_) => "f64",
        FieldType::Bool(_) => "bool",
        FieldType::Date(_) => "date",
        FieldType::Facet(_) => "facet",
        FieldType::Bytes(_) => "bytes",
        FieldType::IpAddr(_) => "ip_addr",
        FieldType::JsonObject(_) => "json",
    }
}

/// Tantivy 文档 → JSON（只读 `STORED` 字段；多值字段取第一个）。
pub(crate) fn to_json(schema: &Schema, document: &TantivyDocument) -> JsonValue {
    let mut object = Map::new();
    for (field, entry) in schema.fields() {
        let Some(value) = document.get_first(field) else {
            continue;
        };

        let json = if let Some(text) = value.as_str() {
            JsonValue::String(text.to_string())
        } else if let Some(number) = value.as_u64() {
            JsonValue::from(number)
        } else if let Some(number) = value.as_i64() {
            JsonValue::from(number)
        } else if let Some(number) = value.as_f64() {
            JsonValue::from(number)
        } else if let Some(flag) = value.as_bool() {
            JsonValue::from(flag)
        } else {
            continue;
        };

        object.insert(entry.name().to_string(), json);
    }
    JsonValue::Object(object)
}

#[cfg(test)]
mod tests {
    use tantivy::schema::{STORED, TEXT};

    use super::*;

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Doc {
        id: u64,
        title: String,
        tags: Vec<String>,
    }

    fn schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_u64_field("id", STORED);
        builder.add_text_field("title", TEXT | STORED);
        builder.add_text_field("tags", TEXT | STORED);
        builder.build()
    }

    fn doc() -> Doc {
        Doc {
            id: 7,
            title: "你好".to_string(),
            tags: vec!["a".to_string(), "b".to_string()],
        }
    }

    #[test]
    fn should_map_document_fields_into_tantivy_doc() {
        let schema = schema();
        let tantivy_doc = to_tantivy(&schema, &doc(), true).unwrap();
        let id = schema.get_field("id").unwrap();
        let title = schema.get_field("title").unwrap();
        let tags = schema.get_field("tags").unwrap();

        assert_eq!(tantivy_doc.get_first(id).and_then(|v| v.as_u64()), Some(7));
        assert_eq!(
            tantivy_doc
                .get_first(title)
                .and_then(|v| v.as_str().map(str::to_string)),
            Some("你好".to_string())
        );
        assert_eq!(tantivy_doc.get_all(tags).count(), 2, "数组应写入为多值字段");
    }

    #[test]
    fn should_reject_unknown_field_in_strict_mode() {
        #[derive(serde::Serialize)]
        struct Extra {
            id: u64,
            unknown: String,
        }

        let schema = schema();
        let extra = Extra {
            id: 1,
            unknown: "typo".to_string(),
        };

        // 严格模式（默认）：拼错的字段名必须报错，否则该字段会被静默丢弃
        let error = to_tantivy(&schema, &extra, true).unwrap_err();
        assert!(error.to_string().contains("unknown"), "实际：{error}");
        // 宽松模式：保持旧行为
        assert!(to_tantivy(&schema, &extra, false).is_ok());
    }

    #[test]
    fn should_reject_type_mismatch_in_strict_mode() {
        #[derive(serde::Serialize)]
        struct Wrong {
            id: String,
            title: String,
            tags: Vec<String>,
        }

        let schema = schema();
        let wrong = Wrong {
            id: "not-a-number".to_string(),
            title: "t".to_string(),
            tags: vec![],
        };

        let error = to_tantivy(&schema, &wrong, true).unwrap_err();
        assert!(error.to_string().contains("id"), "实际：{error}");
        assert!(
            to_tantivy(&schema, &wrong, false).is_ok(),
            "宽松模式仍静默跳过"
        );
    }

    #[test]
    fn should_accept_optional_fields_as_null() {
        #[derive(serde::Serialize)]
        struct Partial {
            id: u64,
            title: Option<String>,
        }

        let schema = schema();
        // 缺失 / null 的字段不报错（字段本身可以不存在）
        let tantivy_doc = to_tantivy(&schema, &Partial { id: 1, title: None }, true).unwrap();
        assert!(
            tantivy_doc
                .get_first(schema.get_field("title").unwrap())
                .is_none()
        );
    }

    #[test]
    fn should_reject_non_object_document() {
        let schema = schema();
        let error = to_tantivy(&schema, &vec![1, 2, 3], true).unwrap_err();
        assert!(error.to_string().contains("JSON 对象"), "实际：{error}");
    }

    #[test]
    fn should_roundtrip_through_json() {
        let schema = schema();
        let tantivy_doc = to_tantivy(&schema, &doc(), true).unwrap();
        let json = to_json(&schema, &tantivy_doc);
        assert_eq!(json["id"], 7);
        assert_eq!(json["title"], "你好");
        assert_eq!(json["tags"], "a", "多值取第一个");
    }
}
