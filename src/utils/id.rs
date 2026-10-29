//! ID 生成工具。

use uuid::Uuid;

/// ID 生成器。
pub struct Id;

impl Id {
    /// 生成一个新的 UUID v4（带连字符）。
    #[must_use]
    pub fn uuid() -> String {
        Uuid::new_v4().to_string()
    }

    /// 生成一个新的 UUID v4（简单形式，32 位十六进制，无连字符）。
    #[must_use]
    pub fn uuid_simple() -> String {
        Uuid::new_v4().simple().to_string()
    }

    /// 生成一个短 ID（取 UUID 前 12 位十六进制）。
    ///
    /// 适用于日志关联等非安全场景；需要密码学强度或全局唯一时请用 [`Id::uuid`]。
    #[must_use]
    pub fn short() -> String {
        let s = Uuid::new_v4().simple().to_string();
        s[..12].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_generate_expected_shapes() {
        assert_eq!(Id::uuid().len(), 36);
        assert_eq!(Id::uuid_simple().len(), 32);
        assert_eq!(Id::short().len(), 12);
    }
}
