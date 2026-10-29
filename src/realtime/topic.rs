//! 频道名校验与日志安全视图。
//!
//! 频道名来自**客户端可控输入**（WS 路径参数 `/ws/{topic}`、SSE 查询参数 `?topic=`），
//! 且 `axum` 的 `Path<String>` / `Query` 都会做 **percent-decode**。因此必须在使用前
//! 收紧，否则两个后果：
//!
//! 1. **日志注入**：`/ws/room%0d%0a[ERROR]...` 解出的频道名带 CRLF，直接进
//!    `tracing` 就能伪造日志行（与 `utils::string::log_safe` 的约定一致，这里统一走它）。
//! 2. **资源放大**：频道名无长度上限时，随机长名可以把频道表撑爆（见 `Hub::subscribe`
//!    的 `max_topics` 治理）。
//!
//! 字符集限定为 `[A-Za-z0-9:_-]` 与 `.`：既能表达 `room:1` / `user.42` 这类常见约定，
//! 又不给控制字符、空白、路径分隔符留口子。

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::utils::string::log_safe;

/// 日志里频道名的展示上限（字符）。
pub const LOG_TOPIC_MAX: usize = 128;

/// 频道名字符是否合法。
fn is_allowed(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-' | b'.')
}

/// 校验频道名。
///
/// # Errors
/// 空串、超长、或含 `[A-Za-z0-9:_-.]` 之外的字节时返回 `10001`。
/// **错误文案不回显频道名本身**——它是用户输入，回显会把注入面搬到响应体里。
pub fn validate(topic: &str, max_len: usize) -> AppResult<()> {
    if topic.is_empty() {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "实时频道名不能为空",
        ));
    }
    if topic.len() > max_len {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            format!("实时频道名过长（上限 {max_len} 字节）"),
        ));
    }
    if !topic.bytes().all(is_allowed) {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "实时频道名只允许 ASCII 字母数字与 `: _ - .`",
        ));
    }
    Ok(())
}

/// 频道名的日志安全视图（控制字符替换 + 截断）。
///
/// 校验过的频道名其实已无控制字符，但**校验失败的分支仍要打日志**（越权探测、畸形
/// 请求），那些输入是未经校验的，所以日志侧统一再过一遍。
#[must_use]
pub fn log_view(topic: &str) -> String {
    log_safe(topic, LOG_TOPIC_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_common_topic_shapes() {
        for topic in ["room:1", "user.42", "a_b-c", "ABC123", "x"] {
            assert!(validate(topic, 128).is_ok(), "{topic} 应通过");
        }
    }

    #[test]
    fn should_reject_empty_and_oversized() {
        assert!(validate("", 128).is_err());
        assert!(validate(&"a".repeat(129), 128).is_err());
        assert!(validate(&"a".repeat(128), 128).is_ok());
    }

    #[test]
    fn should_reject_control_chars_and_separators() {
        // percent-decode 之后 CRLF 会原样出现在字符串里
        for topic in [
            "room\r\nFAKE [ERROR]",
            "room 1",
            "room/1",
            "room\u{0}",
            "频道",
        ] {
            assert!(validate(topic, 128).is_err(), "{topic:?} 应被拒");
        }
    }

    #[test]
    fn should_never_echo_topic_in_error() {
        let error = validate("room\r\nFAKE", 128).unwrap_err();
        let text = format!("{error:?}");
        assert!(!text.contains("FAKE"), "错误里不应回显输入：{text}");
    }

    #[test]
    fn should_sanitize_log_view() {
        assert_eq!(log_view("room\r\nX"), "room  X");
        assert!(log_view(&"a".repeat(200)).chars().count() <= LOG_TOPIC_MAX);
    }
}
