//! 缓存键的构造与校验。
//!
//! 布局：`{namespace}:{class}:{rest}`。`class` 是**唯一保留段**，用于把业务值（`c`）
//! 与语义承载类数据（会话 `s`、幂等 `i`、锁 `l`、队列去重 `q`、实时注册表 `r`、
//! jti 黑名单 `j`、验证码 `o`）隔离：业务键即使写成 `session:x`，也只会落到
//! `{ns}:c:session:x`，不可能命中 `{ns}:s:x`。
//!
//! `rest` 允许包含 `:`（因此 `user:1` 这类既有键无需改写），只限制总长与控制字符。

use crate::errors::{AppError, AppResult};

/// 键的最大字节数（含命名空间与 class 段）。
pub const MAX_KEY_BYTES: usize = 200;

/// 命名空间的最大字节数。
pub const MAX_NAMESPACE_BYTES: usize = 64;

/// 键的语义分类。
///
/// 每个分类对应键布局中的 `class` 段，**互不重叠**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Class {
    /// 业务值（T1 透明缓存：可丢、可重建）。
    Value,
    /// 幂等记录（T2：不可丢）。
    Idempotency,
    /// 分布式锁（T2：互斥语义）。
    Lock,
    /// 会话（T2：不可丢）。
    Session,
    /// 队列消费去重（T2：不可丢）。
    Queue,
    /// 实时实例注册表（T1：尽力而为）。
    Realtime,
    /// jti 黑名单（T2：不可丢）。
    Revocation,
    /// 一次性验证码（T2：需原子消费）。
    Otp,
}

impl Class {
    /// 布局中的 `class` 段取值。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Value => "c",
            Self::Idempotency => "i",
            Self::Lock => "l",
            Self::Session => "s",
            Self::Queue => "q",
            Self::Realtime => "r",
            Self::Revocation => "j",
            Self::Otp => "o",
        }
    }
}

/// 校验命名空间：非空、长度受限、仅 `[A-Za-z0-9_.-]`。
///
/// # Errors
/// 命名空间为空、超长或含非法字符时返回错误描述。
pub fn validate_namespace(namespace: &str) -> Result<(), String> {
    if namespace.is_empty() {
        return Err("[cache].namespace 不能为空（空命名空间会让键空间完全平坦）".to_string());
    }
    if namespace.len() > MAX_NAMESPACE_BYTES {
        return Err(format!(
            "[cache].namespace 超过 {MAX_NAMESPACE_BYTES} 字节上限"
        ));
    }
    if !namespace
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err("[cache].namespace 只允许字母、数字与 `_` `.` `-`".to_string());
    }
    Ok(())
}

/// 按 `{namespace}:{class}:{rest}` 构造键。
///
/// `max_bytes` 由 `[cache].limits.max_key_bytes` 提供，且不会超过 [`MAX_KEY_BYTES`]。
///
/// # Errors
/// `rest` 为空、含控制字符，或拼装后超过上限时返回 `10001`。
pub(crate) fn build(
    namespace: &str,
    class: Class,
    rest: &str,
    max_bytes: usize,
) -> AppResult<String> {
    if rest.is_empty() {
        return Err(AppError::invalid_param("缓存键不能为空"));
    }
    if rest.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(AppError::invalid_param("缓存键不能包含控制字符"));
    }
    let key = if namespace.is_empty() {
        format!("{}:{rest}", class.as_str())
    } else {
        format!("{namespace}:{}:{rest}", class.as_str())
    };
    if key.len() > max_bytes {
        return Err(AppError::invalid_param(format!(
            "缓存键超过 {max_bytes} 字节上限"
        )));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_isolate_value_keys_from_semantic_classes() {
        let value = build("app", Class::Value, "session:abc", MAX_KEY_BYTES).unwrap();
        let session = build("app", Class::Session, "abc", MAX_KEY_BYTES).unwrap();
        assert_eq!(value, "app:c:session:abc");
        assert_eq!(session, "app:s:abc");
        assert_ne!(value, session);
    }

    #[test]
    fn should_keep_colon_inside_rest() {
        assert_eq!(
            build("app", Class::Value, "user:1", MAX_KEY_BYTES).unwrap(),
            "app:c:user:1"
        );
    }

    #[test]
    fn should_reject_empty_and_control_chars() {
        assert!(build("app", Class::Value, "", MAX_KEY_BYTES).is_err());
        assert!(build("app", Class::Value, "a\nb", MAX_KEY_BYTES).is_err());
        assert!(build("app", Class::Value, "a\u{7f}b", MAX_KEY_BYTES).is_err());
    }

    #[test]
    fn should_reject_overlong_keys() {
        let long = "k".repeat(MAX_KEY_BYTES);
        assert!(build("app", Class::Value, &long, MAX_KEY_BYTES).is_err());
        assert!(build("app", Class::Value, "short", 8).is_err());
    }

    #[test]
    fn should_validate_namespace() {
        assert!(validate_namespace("aurora").is_ok());
        assert!(validate_namespace("app.v2-test_1").is_ok());
        assert!(validate_namespace("").is_err());
        assert!(validate_namespace("a b").is_err());
        assert!(validate_namespace("a:b").is_err());
        assert!(validate_namespace(&"a".repeat(MAX_NAMESPACE_BYTES + 1)).is_err());
    }
}
