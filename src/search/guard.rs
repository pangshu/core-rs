//! 查询串预检（**在**交给 Tantivy 解析之前）。
//!
//! 这里挡两类东西，都是 `QueryParser` 本身不管的：
//!
//! 1. **资源放大**：超长查询串、超深括号嵌套（解析器是递归下降，无深度上限会栈溢出，
//!    栈溢出是 abort 而不是 panic，`catch_panic` 接不住）、通配/模糊（全词典扫描）。
//! 2. **字段越权**：`Query::fields` 只是「未限定字段时的默认字段」，用户自己写
//!    `body:机密` 时 Tantivy 会走 `Schema::find_field` 解析到**任意**索引字段
//!    （`QueryParser::compute_path_triplets_for_literal`），所以必须在解析前按
//!    允许字段集合拦一遍。

use std::collections::HashSet;

use tantivy::schema::{Field, Schema};

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::search::options::SearchOptions;

/// Tantivy 语法里的特殊字符（与 `tantivy-query-grammar` 的 `SPECIAL_CHARS` 一致）。
///
/// 字段名不能包含它们，因此反向扫描到其中任意一个就说明字段名结束。
const SPECIAL_CHARS: &[char] = &[
    '+', '^', '`', ':', '{', '}', '"', '\'', '[', ']', '(', ')', '!', '\\', '*', ' ',
];

/// 校验查询串的长度 / 嵌套深度 / 通配语法。
pub(crate) fn check_text(text: &str, options: &SearchOptions) -> AppResult<()> {
    let len = text.chars().count();
    if len > options.query_max_len {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            format!("查询串过长（{len} > {} 字符）", options.query_max_len),
        ));
    }

    if !options.allow_wildcard && text.contains(['*', '~']) {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "查询串不允许通配/模糊语法",
        ));
    }

    let mut depth = 0_usize;
    let mut in_quote = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => in_quote = !in_quote,
            '(' if !in_quote => {
                depth += 1;
                if depth > options.query_max_depth {
                    return Err(AppError::biz(
                        ErrorCode::INVALID_PARAM,
                        format!("查询串括号嵌套过深（上限 {} 层）", options.query_max_depth),
                    ));
                }
            }
            ')' if !in_quote => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// 校验查询串里显式写出的字段名都在允许集合内。
///
/// 只拦「解析得到 schema 字段、但不在允许集合」的情况；解析不到字段名时交给
/// Tantivy 自己报 `FieldDoesNotExist`（保持原有错误语义）。
pub(crate) fn check_fields(text: &str, schema: &Schema, allowed: &HashSet<Field>) -> AppResult<()> {
    let chars: Vec<char> = text.chars().collect();
    let mut in_quote = false;
    let mut escaped = false;

    for (index, &c) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                continue;
            }
            '"' => {
                in_quote = !in_quote;
                continue;
            }
            ':' if !in_quote => {}
            _ => continue,
        }

        // 从 `:` 往前收字段名：遇到特殊字符/空白即结束（与 `field_name` 语法一致）。
        let mut start = index;
        while start > 0 {
            let prev = chars[start - 1];
            if SPECIAL_CHARS.contains(&prev) || prev.is_whitespace() {
                break;
            }
            start -= 1;
        }
        if start == index {
            continue;
        }
        let name: String = chars[start..index].iter().collect();
        // `-body:x` 是「排除」运算符 + 字段名，`-` 不是特殊字符（语法只禁止它出现在
        // 字段名首位），所以要把前缀的排除符剥掉再判断——否则 `-body:secret` 就绕过了。
        let name = name.trim_start_matches('-');
        if name.is_empty() {
            continue;
        }

        let Some((field, _path)) = schema.find_field(name) else {
            continue;
        };
        if !allowed.contains(&field) {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("查询字段 {name} 不在允许范围内"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tantivy::schema::{STORED, Schema, TEXT};

    use super::*;
    use crate::config::SearchSection;

    fn options() -> SearchOptions {
        SearchOptions::from_config(&SearchSection::default())
    }

    fn schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_text_field("title", TEXT | STORED);
        builder.add_text_field("body", TEXT | STORED);
        builder.add_u64_field("secret_id", STORED);
        builder.build()
    }

    fn allowed(schema: &Schema, names: &[&str]) -> HashSet<Field> {
        names
            .iter()
            .map(|name| schema.get_field(name).unwrap())
            .collect()
    }

    #[test]
    fn should_accept_normal_query() {
        assert!(check_text("hello world", &options()).is_ok());
        assert!(check_text("(a AND b) OR c", &options()).is_ok());
    }

    #[test]
    fn should_reject_overlong_query() {
        let text = "a".repeat(513);
        let error = check_text(&text, &options()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
    }

    #[test]
    fn should_reject_deep_nesting() {
        // 17 层就超过默认上限 16（Tantivy 解析器是递归下降，深嵌套会栈溢出）
        let text = format!("{}x{}", "(".repeat(17), ")".repeat(17));
        let error = check_text(&text, &options()).unwrap_err();
        assert!(error.to_string().contains("嵌套"), "实际：{error}");
    }

    #[test]
    fn should_ignore_parens_inside_phrase() {
        let text = format!("\"{}\"", "(".repeat(40));
        assert!(
            check_text(&text, &options()).is_ok(),
            "引号内的括号是字面量"
        );
    }

    #[test]
    fn should_reject_wildcard_when_disabled() {
        let mut opts = options();
        opts.allow_wildcard = false;
        assert!(check_text("hello*", &opts).is_err());
        assert!(check_text("helo~2", &opts).is_err());
        assert!(check_text("hello", &opts).is_ok());
    }

    #[test]
    fn should_reject_field_outside_allowlist() {
        let schema = schema();
        let allow = allowed(&schema, &["title"]);
        let error = check_fields("body:机密", &schema, &allow).unwrap_err();
        assert!(error.to_string().contains("body"), "实际：{error}");

        // 数字字段的区间探测同样被拦（这是最典型的「用命中数当 oracle」的入口）
        assert!(check_fields("secret_id:[1 TO 100]", &schema, &allow).is_err());
    }

    #[test]
    fn should_allow_listed_field() {
        let schema = schema();
        let allow = allowed(&schema, &["title", "body"]);
        assert!(check_fields("title:hello body:world", &schema, &allow).is_ok());
        assert!(check_fields("title:\"a b\"", &schema, &allow).is_ok());
    }

    #[test]
    fn should_ignore_escaped_colon_and_unknown_field() {
        let schema = schema();
        let allow = allowed(&schema, &["title"]);
        // 转义冒号是字面量，不是字段限定
        assert!(check_fields(r"a\:b", &schema, &allow).is_ok());
        // 解析不到的字段名交给 Tantivy 报 FieldDoesNotExist
        assert!(check_fields("nope:value", &schema, &allow).is_ok());
        // 引号内的 `body:` 是字面量
        assert!(check_fields("\"body:secret\"", &schema, &allow).is_ok());
    }

    #[test]
    fn should_not_be_bypassed_by_occur_operators() {
        let schema = schema();
        let allow = allowed(&schema, &["title"]);

        // `-body:x` 是「排除 body 命中 x 的文档」，仍然会读 body 的词项，
        // 同样能当 oracle 用，必须一并拦下
        assert!(check_fields("-body:secret", &schema, &allow).is_err());
        assert!(check_fields("+body:secret", &schema, &allow).is_err());
        assert!(check_fields("title:a -body:b", &schema, &allow).is_err());
        // 字段名本身含 `-` 是合法的（只禁止出现在首位）
        assert!(check_fields("a-b:c", &schema, &allow).is_ok());
    }

    #[test]
    fn should_ignore_url_like_terms() {
        let schema = schema();
        let allow = allowed(&schema, &["title"]);
        assert!(check_fields("https://example.com", &schema, &allow).is_ok());
    }
}
