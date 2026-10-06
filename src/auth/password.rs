//! argon2 密码哈希（文档 三·13：三种认证方式共用）：`hash` 生成 PHC 格式串
//! （含随机盐），`verify` 校验。密码策略校验见 `[auth.password]` 的
//! `PasswordPolicy::check`。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;

use crate::error::{AppError, AppResult};

/// 哈希密码，返回 PHC 格式字符串（可直接入库）
pub fn hash(password: &str) -> AppResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::internal(format!("password hash failed: {e}")))
}

/// 校验密码与哈希是否匹配
pub fn verify(password: &str, hash: &str) -> AppResult<bool> {
    let parsed = PasswordHash::new(hash)
        .map_err(|e| AppError::internal(format!("invalid password hash: {e}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let h = hash("s3cret-Pass").unwrap();
        assert!(h.starts_with("$argon2"));
        assert!(verify("s3cret-Pass", &h).unwrap());
        assert!(!verify("wrong", &h).unwrap());
    }
}
