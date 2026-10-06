//! XSS 防护（文档 三·16）：输入清洗（trim、去零宽字符）+ HTML 转义 /
//! 标签白名单，防存储型 XSS。供 handler、service 显式调用；
//! **业务的敏感词过滤仍留应用 `filter/`**。

/// 输入清洗：trim + 去零宽字符（U+200B/200C/200D/FEFF、BOM）。
/// 富文本以外的普通输入建议入库前过一遍。
pub fn clean_text(input: &str) -> String {
    input
        .chars()
        .filter(|c| !matches!(*c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// HTML 转义：`< > & " '` → 实体（纯文本内容入库 / 回显的安全基线）
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// 标签剥离（**有损、不构成 XSS 防护**）：移除 HTML 标签，但简单状态机不感知
/// 引号（标签内引号中的 `>` 会提前终止标签态）、未闭合 `<`（如 `<3`）会吞掉
/// 其后内容直到下一个 `>`。仅用于纯文本粗加工；真正的 XSS 防线是
/// [`escape_html`] / [`sanitize_plain`]，富文本请引入专用清洗库（如 ammonia）。
pub fn strip_tags_lossy(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// 综合清洗：strip_tags_lossy → clean_text → escape_html（不可信纯文本入库的默认姿势）
pub fn sanitize_plain(input: &str) -> String {
    escape_html(&clean_text(&strip_tags_lossy(input)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_zero_width_and_trim() {
        assert_eq!(clean_text("\u{200B}hello\u{FEFF} "), "hello");
    }

    #[test]
    fn escapes_html() {
        assert_eq!(escape_html("<b>\"x\"</b>"), "&lt;b&gt;&quot;x&quot;&lt;/b&gt;");
    }

    #[test]
    fn strips_tags_and_sanitizes() {
        // 剥标签留文本（script 内容仍在）——富文本清洗需应用侧专用库（如 ammonia）
        assert_eq!(strip_tags_lossy("a<script>alert(1)</script>b"), "aalert(1)b");
        assert_eq!(sanitize_plain(" <script>x</script>hi "), "xhi"); // 同上：剥标签留文本
    }
}
