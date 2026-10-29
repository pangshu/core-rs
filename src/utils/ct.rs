//! 常量时间比较。
//!
//! 放在 `utils`（常开）而不是 `security`（可选 feature）：CSRF 双提交 Cookie、
//! metrics 抓取令牌、限流主体键都要求「比较过程不随内容提前返回」，而它们各自
//! 对应的 feature（`csrf` / `metrics` / `ratelimit`）不蕴含 `security`。
//! 曾经三处各写一遍手写循环——行为一致但**没有一处是可证明的常量时间**，
//! 且修一处漏两处。
//!
//! # 边界
//!
//! **只在长度相等时是常量时间的**：长度不等会立即返回 `false`（长度本身会泄漏）。
//! 校验 MAC / token 时应保证长度固定，或比较固定长度的摘要值。

use subtle::ConstantTimeEq;

/// 常量时间比较（防时序侧信道）。
///
/// ```
/// # use aurora::utils::constant_time_eq;
/// assert!(constant_time_eq(b"abc", b"abc"));
/// assert!(!constant_time_eq(b"abc", b"abd"));
/// ```
#[must_use]
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.ct_eq(right).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compare_equal_and_unequal() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        // 长度不等：内容不同、长度不同都必须为 false
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
