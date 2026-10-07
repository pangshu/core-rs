//! argon2 密码哈希（文档 三·13：三种认证方式共用）：`hash` 生成 PHC 格式串
//! （含随机盐），`verify` 校验。密码策略校验见 `[auth.password]` 的
//! `PasswordPolicy::check`。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString}; // 引入 argon2 的 PHC 哈希解析/生成与校验接口及盐类型
use argon2::Argon2; // 引入 Argon2 主类型（默认参数即安全参数）
use argon2::password_hash::rand_core::OsRng; // 引入操作系统安全随机数源，用于生成盐

use crate::error::{AppError, AppResult}; // 引入框架统一错误类型与结果别名

/// 哈希密码，返回 PHC 格式字符串（可直接入库）
pub fn hash(password: &str) -> AppResult<String> { // 定义密码哈希函数，输出可直接入库的 PHC 串
    let salt = SaltString::generate(&mut OsRng); // 用系统随机源生成随机盐，避免彩虹表
    Argon2::default() // 使用 Argon2 默认（推荐）参数构造哈希器
        .hash_password(password.as_bytes(), &salt) // 以字节形式对密码加盐哈希
        .map(|h| h.to_string()) // 成功时把 PHC 结构转成字符串（含算法/参数/盐/哈希）
        .map_err(|e| AppError::internal(format!("password hash failed: {e}"))) // 失败时包装为 500 内部错误
}

/// 校验密码与哈希是否匹配
pub fn verify(password: &str, hash: &str) -> AppResult<bool> { // 定义密码校验函数，比对明文与存储哈希
    let parsed = PasswordHash::new(hash) // 解析库中存储的 PHC 格式哈希串
        .map_err(|e| AppError::internal(format!("invalid password hash: {e}")))?; // 解析失败说明库中数据损坏，返回内部错误
    Ok(Argon2::default() // 用相同默认参数构造校验器
        .verify_password(password.as_bytes(), &parsed) // 按解析出的盐与参数重算并比对
        .is_ok()) // 比对成功返回 true，不匹配返回 false（不泄露原因）
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
