//! XSS 防护（文档 三·16）：输入清洗（trim、去零宽字符）+ HTML 转义 /
//! 标签白名单，防存储型 XSS。供 handler、service 显式调用；
//! **业务的敏感词过滤仍留应用 `filter/`**。

/// 输入清洗：trim + 去零宽字符（U+200B/200C/200D/FEFF、BOM）。
/// 富文本以外的普通输入建议入库前过一遍。
pub fn clean_text(input: &str) -> String { // 清洗普通文本输入：去零宽字符并去除首尾空白
    input // 从输入字符串开始链式处理
        .chars() // 转成字符迭代器（按 Unicode 标量逐个处理）
        .filter(|c| !matches!(*c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}')) // 过滤掉零宽空格/连接符与 BOM 等不可见字符
        .collect::<String>() // 把剩余字符收集回 String
        .trim() // 去除首尾空白
        .to_string() // 转为拥有所有权的 String 返回
}

/// HTML 转义：`< > & " '` → 实体（纯文本内容入库 / 回显的安全基线）
pub fn escape_html(input: &str) -> String { // 对 HTML 特殊字符做实体转义
    let mut out = String::with_capacity(input.len()); // 预分配输出缓冲（转义后长度只会增长）
    for c in input.chars() { // 逐字符遍历输入
        match c { // 对每个字符分派处理
            '<' => out.push_str("&lt;"), // 小于号转义为 &lt;
            '>' => out.push_str("&gt;"), // 大于号转义为 &gt;
            '&' => out.push_str("&amp;"), // & 转义为 &amp;
            '"' => out.push_str("&quot;"), // 双引号转义为 &quot;
            '\'' => out.push_str("&#39;"), // 单引号转义为 &#39;
            other => out.push(other), // 其余字符原样追加
        }
    }
    out // 返回转义后的字符串
}

/// 标签剥离（**有损、不构成 XSS 防护**）：移除 HTML 标签，但简单状态机不感知
/// 引号（标签内引号中的 `>` 会提前终止标签态）、未闭合 `<`（如 `<3`）会吞掉
/// 其后内容直到下一个 `>`。仅用于纯文本粗加工；真正的 XSS 防线是
/// [`escape_html`] / [`sanitize_plain`]，富文本请引入专用清洗库（如 ammonia）。
pub fn strip_tags_lossy(input: &str) -> String { // 用简单状态机剥离 HTML 标签（有损、非安全防线）
    let mut out = String::with_capacity(input.len()); // 预分配输出缓冲
    let mut in_tag = false; // 标记当前是否处于标签内部
    for c in input.chars() { // 逐字符遍历输入
        match c { // 对每个字符分派处理
            '<' => in_tag = true, // 遇到 < 进入标签态
            '>' => in_tag = false, // 遇到 > 退出标签态
            c if !in_tag => out.push(c), // 标签外的字符保留到输出
            _ => {} // 标签内其余字符丢弃
        }
    }
    out // 返回剥标签后的字符串
}

/// 综合清洗：strip_tags_lossy → clean_text → escape_html（不可信纯文本入库的默认姿势）
pub fn sanitize_plain(input: &str) -> String { // 不可信纯文本的默认清洗组合
    escape_html(&clean_text(&strip_tags_lossy(input))) // 依次剥标签、去零宽并转义 HTML
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
