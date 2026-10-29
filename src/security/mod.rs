//! 安全模块（feature `security`）。
//!
//! 提供密码哈希、对称/非对称加解密、签名与敏感值包装（见 `docs/11` §4 / §6）。
//!
//! - [`SecurityContext`]：`AppState.security` 的装配结果（加密器 / HMAC / Argon2 参数）
//! - [`crypto`]：AEAD（带版本头与 AAD）、HMAC、RSA、常量时间比较
//! - [`password`]：Argon2id 哈希与等时校验
//! - [`secret`]：敏感值包装（掩码 + 清零）

pub mod context;
pub mod crypto;
pub mod password;
pub mod secret;

pub use context::SecurityContext;
pub use crypto::{
    AesGcmCipher, ChaChaCipher, Cipher, HmacSigner, RsaSigner, RsaVerifier, constant_time_eq,
};
pub use password::{
    Argon2Params, VerifyOutcome, hash_password, verify_password, verify_password_or_dummy,
};
pub use secret::Secret;
