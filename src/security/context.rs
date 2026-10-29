//! 安全能力的装配容器（`AppState.security`）。
//!
//! 把 `[security]` 段里的三样东西（对称加密器 / HMAC 签名器 / Argon2 参数）
//! 一次性装配好，并提供**fail-closed 的取用入口**：
//!
//! - 配置了就用配置的；
//! - 没配置就报错，**绝不**回退到「全零密钥」或「空 HMAC 密钥」——
//!   `AesGcmCipher::from_slice("")` 曾经能拿到一把全零密钥并「加密成功」，
//!   这类降级比不加密更危险。
//!
//! 需要「有就用、没有就算了」的场景用 [`SecurityContext::cipher_opt`] /
//! [`SecurityContext::hmac_opt`]，把「未配置」这件事显式写进业务分支。

use std::fmt;

use crate::config::sections::security::SecuritySection;
use crate::errors::{AppError, AppResult};
use crate::security::crypto::{Cipher, HmacSigner};
use crate::security::password::Argon2Params;

/// 安全能力集合。
#[derive(Clone)]
pub struct SecurityContext {
    cipher: Option<Cipher>,
    hmac: Option<HmacSigner>,
    argon2: Argon2Params,
}

impl fmt::Debug for SecurityContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecurityContext")
            .field("cipher", &self.cipher)
            .field("hmac", &self.hmac)
            .field("argon2", &self.argon2)
            .finish()
    }
}

impl SecurityContext {
    /// 按 `[security]` 装配。
    ///
    /// 密钥留空表示「该能力未启用」（`None`），不是「用空密钥」。
    ///
    /// # Errors
    /// 密钥长度不足、`crypto.previous_key` 单独出现、Argon2 参数越界时返回
    /// `10001`——启动期由 `AppState::build` 转成配置错误并退出。
    pub fn from_config(section: &SecuritySection) -> AppResult<Self> {
        Ok(Self {
            cipher: Cipher::from_config(section)?,
            hmac: HmacSigner::from_config(section)?,
            argon2: Argon2Params::from_config(&section.password)?,
        })
    }

    /// 取对称加密器；未配置即报错（fail-closed）。
    ///
    /// # Errors
    /// `[security].crypto.key` 未配置时返回 `50001`（细节进日志，不回显给客户端）。
    pub fn cipher(&self) -> AppResult<&Cipher> {
        self.cipher.as_ref().ok_or_else(|| {
            AppError::internal(anyhow::anyhow!(
                "[security].crypto.key 未配置：对称加密不可用（不会降级为全零密钥）"
            ))
        })
    }

    /// 取对称加密器；未配置返回 `None`（供「有就用」的分支显式判断）。
    #[must_use]
    pub fn cipher_opt(&self) -> Option<&Cipher> {
        self.cipher.as_ref()
    }

    /// 取 HMAC 签名器；未配置即报错（fail-closed）。
    ///
    /// # Errors
    /// `[security].hmac.key` 未配置时返回 `50001`。
    pub fn hmac(&self) -> AppResult<&HmacSigner> {
        self.hmac.as_ref().ok_or_else(|| {
            AppError::internal(anyhow::anyhow!(
                "[security].hmac.key 未配置：HMAC 签名不可用（不会降级为空密钥）"
            ))
        })
    }

    /// 取 HMAC 签名器；未配置返回 `None`。
    #[must_use]
    pub fn hmac_opt(&self) -> Option<&HmacSigner> {
        self.hmac.as_ref()
    }

    /// Argon2id 参数（始终可用，缺省为推荐值）。
    #[must_use]
    pub fn argon2(&self) -> &Argon2Params {
        &self.argon2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sections::security::{CryptoSecurity, HmacSecurity, PasswordSecurity};
    use crate::errors::ErrorCode;

    #[test]
    fn should_leave_capabilities_disabled_when_keys_are_empty() {
        let context = SecurityContext::from_config(&SecuritySection::default()).unwrap();
        assert!(context.cipher_opt().is_none());
        assert!(context.hmac_opt().is_none());
        assert_eq!(context.argon2(), &Argon2Params::default());
    }

    #[test]
    fn should_fail_closed_when_cipher_is_not_configured() {
        let context = SecurityContext::from_config(&SecuritySection::default()).unwrap();
        let error = context.cipher().unwrap_err();
        assert_eq!(error.code(), ErrorCode::INTERNAL);
        // 细节只在日志侧可见
        assert!(
            error
                .inner()
                .is_some_and(|e| e.to_string().contains("crypto.key"))
        );
    }

    #[test]
    fn should_fail_closed_when_hmac_is_not_configured() {
        let context = SecurityContext::from_config(&SecuritySection::default()).unwrap();
        assert_eq!(context.hmac().unwrap_err().code(), ErrorCode::INTERNAL);
    }

    #[test]
    fn should_build_both_capabilities_from_config() {
        let section = SecuritySection {
            crypto: CryptoSecurity {
                key: "0123456789abcdef0123456789abcdef".to_string(),
                ..CryptoSecurity::default()
            },
            hmac: HmacSecurity {
                key: "fedcba9876543210fedcba9876543210".to_string(),
            },
            password: PasswordSecurity::default(),
        };
        let context = SecurityContext::from_config(&section).unwrap();
        assert!(context.cipher().is_ok());
        assert!(context.hmac().is_ok());
    }

    #[test]
    fn should_reject_short_keys_at_startup() {
        let section = SecuritySection {
            crypto: CryptoSecurity {
                key: "short".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(SecurityContext::from_config(&section).is_err());
    }

    #[test]
    fn should_mask_secrets_in_debug() {
        let section = SecuritySection {
            crypto: CryptoSecurity {
                key: "super-secret-crypto-key-material".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        let text = format!("{:?}", SecurityContext::from_config(&section).unwrap());
        assert!(!text.contains("super-secret"), "Debug 不得泄露密钥：{text}");
    }
}
