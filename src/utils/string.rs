//! 字符串工具。

/// 按**字符数**截断，超出部分以 `…` 结尾；不会切断 UTF-8 码点。
#[must_use]
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 把不可信文本转成**可安全写入单行日志**的形式。
///
/// 错误文本常来自对端服务（数据库 / Redis / 上游 HTTP）或用户输入，
/// 其中的换行与其它控制字符能把一条日志伪造成多行（**日志注入**）。
/// 这里把控制字符统一替换为空格，并把总长度压到 `max_chars` 以内（超出以 `…` 结尾）。
#[must_use]
pub fn log_safe(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let mut out = String::with_capacity(s.len().min(max_chars.saturating_mul(4)));
    let mut truncated = false;
    for (index, ch) in s.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }

    if truncated {
        if out.chars().count() >= max_chars {
            out = out.chars().take(max_chars - 1).collect();
        }
        out.push('…');
    }
    out
}

/// 抹掉 URL 里的凭据（`scheme://user:pass@host` → `scheme://***@host`）。
///
/// 建连失败时驱动原文常常带完整连接串，而 `StartupError::Resource` 会被原样打进
/// 启动日志。日志里出现数据库 / Redis 口令，等于把凭据写进了日志采集系统。
///
/// 只处理 `://...@` 这一段：不改 scheme / host / path（排障要用），也不做 URL 解析
/// （畸形串也要能脱敏，解析失败时宁可多抹一点）。
#[must_use]
pub fn redact_url_secrets(input: &str) -> String {
    let Some((scheme, rest)) = input.split_once("://") else {
        return input.to_string();
    };
    // userinfo 必须在第一个 '/' 之前（`host/path` 里的 '@' 不是凭据）
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_userinfo, host)) => format!("{scheme}://***@{host}{tail}"),
        None => input.to_string(),
    }
}

/// 掩码字符串：保留前 `keep_prefix` 与后 `keep_suffix` 个字符，中间以 `***` 替代。
#[must_use]
pub fn mask(s: &str, keep_prefix: usize, keep_suffix: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= keep_prefix + keep_suffix {
        return "***".to_string();
    }
    let head: String = chars[..keep_prefix].iter().collect();
    let tail: String = chars[chars.len() - keep_suffix..].iter().collect();
    format!("{head}***{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_truncate_by_chars() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 4), "hel…");
        assert_eq!(truncate_chars("中文测试", 3), "中文…");
    }

    #[test]
    fn should_redact_url_credentials() {
        assert_eq!(
            redact_url_secrets("postgres://app:s3cret@db.internal:5432/app"),
            "postgres://***@db.internal:5432/app"
        );
        assert_eq!(
            redact_url_secrets("rediss://:pw@redis:6379/0"),
            "rediss://***@redis:6379/0"
        );
        // 无凭据 / 非 URL：原样返回
        assert_eq!(redact_url_secrets("sqlite::memory:"), "sqlite::memory:");
        assert_eq!(
            redact_url_secrets("postgres://db.internal/app"),
            "postgres://db.internal/app"
        );
        // path 里的 '@' 不是凭据
        assert_eq!(redact_url_secrets("https://h/a@b"), "https://h/a@b");
    }

    #[test]
    fn should_mask_middle() {
        assert_eq!(mask("1234567890", 3, 2), "123***90");
        assert_eq!(mask("abc", 2, 2), "***");
    }

    #[test]
    fn should_flatten_control_chars_when_logging() {
        // 换行 / 制表符被压成空格，日志无法被伪造成多行
        assert_eq!(log_safe("a\nb\tc", 64), "a b c");
        assert_eq!(log_safe("a\r\nb", 64), "a  b");
    }

    #[test]
    fn should_truncate_log_safe_text() {
        assert_eq!(log_safe("abcdef", 3), "ab…");
        assert_eq!(log_safe("abcdef", 64), "abcdef");
    }
}
