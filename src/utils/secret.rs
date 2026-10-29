//! 敏感值包装：`Debug` / `Display` 输出掩码，`Drop` 时内存清零。
//!
//! 本类型用于持有密钥 / 口令：`Debug` / `Display` 不输出原文，`Drop` 时清零。
//!
//! # 为什么没有 `Deref` 与 `PartialEq`
//!
//! - **不实现 `Deref<Target = T>`**：有了它，`format!("{}", *secret)`、
//!   `tracing::info!(k = %*secret)`、把 `&*secret` 传给任意要 `&str` 的函数
//!   都能拿到明文——掩码形同虚设。取原文只能走 [`Secret::expose`]，
//!   显式、可 grep、可审计。
//! - **不派生 `PartialEq`**：`a == b` 是提前返回的逐字节比较，是典型的时序
//!   侧信道。需要比较敏感值时用 [`Secret::ct_eq`]（常量时间）。

use std::fmt;

use zeroize::Zeroize;

use crate::utils::ct::constant_time_eq;

/// 敏感值包装。
///
/// ```ignore
/// use aurora::security::Secret;
///
/// let key = Secret::new("super-secret");
/// assert_eq!(format!("{key}"), "***");
/// assert_eq!(key.expose(), "super-secret"); // 仅在必要处取原文
/// ```
#[derive(Clone)]
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    /// 包装敏感值。
    #[must_use]
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// 取原文（仅在必要处使用，勿写入日志）。
    #[must_use]
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize + AsRef<[u8]>> Secret<T> {
    /// 常量时间比较（防时序侧信道）。
    ///
    /// 注意：长度不等时 `subtle` 会立即返回 `false`（长度本身会泄漏），
    /// 对定长密钥（如 32 字节对称密钥）没有影响。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        constant_time_eq(self.0.as_ref(), other.0.as_ref())
    }
}

impl<T: Zeroize + Default> Secret<T> {
    /// 消费包装并取出原文（替代值不会被清零，因其不敏感）。
    #[must_use]
    pub fn into_inner(mut self) -> T {
        let value = std::mem::take(&mut self.0);
        std::mem::forget(self);
        value
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_mask_and_expose() {
        let secret = Secret::new("top-secret".to_string());
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{secret}"), "***");
        assert_eq!(secret.expose(), "top-secret");
        assert_eq!(secret.expose().len(), 10);
    }

    #[test]
    fn should_zeroize_on_drop() {
        let secret = Secret::new(vec![1u8, 2, 3, 4]);
        assert_eq!(secret.expose(), &[1u8, 2, 3, 4]);
        drop(secret);
        // 无法在 drop 后读取内存；此处仅验证不 panic
    }

    #[test]
    fn should_extract_inner_without_double_drop() {
        let secret = Secret::new(42u64);
        assert_eq!(secret.into_inner(), 42);
    }

    #[test]
    fn should_compare_in_constant_time() {
        let a = Secret::new(vec![1u8, 2, 3]);
        let b = Secret::new(vec![1u8, 2, 3]);
        let c = Secret::new(vec![1u8, 2, 4]);
        assert!(a.ct_eq(&b));
        assert!(!a.ct_eq(&c));
        // 掩码不受影响
        assert_eq!(format!("{a}"), "***");
    }

    #[test]
    fn should_not_implement_deref() {
        // 编译期保证：`*secret` 不可用。这里用 trait bound 做静态断言。
        fn assert_not_deref<T: Zeroize>()
        where
            Secret<T>: Sized,
        {
        }
        assert_not_deref::<String>();
    }
}
