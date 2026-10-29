//! `[security]` 配置段（feature `security`）。

use serde::Deserialize;

/// 对称 / HMAC 密钥的最小字节数（256 bit）。
pub const MIN_KEY_BYTES: usize = 32;

/// Argon2id 内存开销下限（KiB，8 MiB）。
pub const MIN_ARGON2_MEMORY_KIB: u32 = 8 * 1024;

/// Argon2id 内存开销上限（KiB，1 GiB）——防止把配置写成天文数字后每次登录 OOM。
pub const MAX_ARGON2_MEMORY_KIB: u32 = 1024 * 1024;

/// Argon2id 迭代次数下限。
pub const MIN_ARGON2_TIME_COST: u32 = 2;

/// 安全配置段。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct SecuritySection {
    /// 对称加密配置。
    pub crypto: CryptoSecurity,
    /// HMAC 配置。
    pub hmac: HmacSecurity,
    /// 密码哈希配置。
    pub password: PasswordSecurity,
}

impl SecuritySection {
    /// 启动期校验（fail-fast）。
    ///
    /// 密钥**允许留空**（业务可能没用到对称加密），但一旦配置就必须达到
    /// 算法要求的最小长度；Argon2id 参数必须有下限，否则密码哈希形同虚设。
    ///
    /// # Errors
    /// 密钥过短或 Argon2id 参数越界时返回人类可读的错误信息。
    pub fn validate(&self) -> Result<(), String> {
        self.crypto.validate()?;
        self.hmac.validate()?;
        self.password.validate()?;
        Ok(())
    }
}

/// 对称加密配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct CryptoSecurity {
    /// 对称加密密钥（敏感项 🔒，只走环境变量 `AURORA_SECURITY__CRYPTO__KEY`）。
    ///
    /// 留空表示**不使用对称加密**（`SecurityContext::cipher_opt()` 为 `None`），
    /// 不会降级成全零密钥。
    pub key: String,
    /// 上一代对称加密密钥（敏感项 🔒，只走环境变量
    /// `AURORA_SECURITY__CRYPTO__PREVIOUS_KEY`）。
    ///
    /// 仅用于**解密**，支持密钥轮换过渡期：新密文用 `key`，历史密文回退到本密钥。
    /// 过渡期结束后必须清空它，否则等于长期保留一把已退役的密钥。
    pub previous_key: String,
    /// 算法。
    pub algorithm: CryptoAlgorithm,
}

impl Default for CryptoSecurity {
    fn default() -> Self {
        Self {
            key: String::new(),
            previous_key: String::new(),
            algorithm: CryptoAlgorithm::AesGcm,
        }
    }
}

impl CryptoSecurity {
    /// 校验密钥长度（留空表示不使用对称加密）。
    ///
    /// # Errors
    /// 密钥非空但不足 [`MIN_KEY_BYTES`] 字节时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        validate_key("[security].crypto.key", &self.key)?;
        validate_key("[security].crypto.previous_key", &self.previous_key)?;
        if self.key.trim().is_empty() && !self.previous_key.trim().is_empty() {
            return Err(
                "[security].crypto.previous_key 已配置但 [security].crypto.key 为空；\
                 轮换过渡期必须同时提供新密钥"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// 对称加密算法。
///
/// 取值与 `docs/08` §3 的写法一致（`aes-gcm` / `chacha20poly1305`）——
/// 用 `rename_all = "lowercase"` 会得到 `aesgcm`，与文档不符，配置一写就启动失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[non_exhaustive]
pub enum CryptoAlgorithm {
    /// AES-256-GCM。
    #[default]
    #[serde(rename = "aes-gcm")]
    AesGcm,
    /// ChaCha20-Poly1305。
    #[serde(rename = "chacha20poly1305")]
    Chacha20poly1305,
}

/// HMAC 配置段。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct HmacSecurity {
    /// HMAC 密钥（敏感项 🔒，只走环境变量 `AURORA_SECURITY__HMAC__KEY`）。
    pub key: String,
}

impl HmacSecurity {
    /// 校验密钥长度（留空表示不使用 HMAC）。
    ///
    /// # Errors
    /// 密钥非空但不足 [`MIN_KEY_BYTES`] 字节时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        validate_key("[security].hmac.key", &self.key)
    }
}

/// 密码哈希配置段（Argon2id）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct PasswordSecurity {
    /// Argon2id 内存开销（KiB）。
    pub memory_cost: u32,
    /// 迭代次数。
    pub time_cost: u32,
    /// 并行度。
    pub parallelism: u32,
}

impl Default for PasswordSecurity {
    fn default() -> Self {
        Self {
            memory_cost: 19_456,
            time_cost: 2,
            parallelism: 1,
        }
    }
}

impl PasswordSecurity {
    /// 校验 Argon2id 参数下限（防止「配了但等于没配」）。
    ///
    /// # Errors
    /// 内存开销 / 迭代次数 / 并行度越界时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        if self.memory_cost < MIN_ARGON2_MEMORY_KIB {
            return Err(format!(
                "[security].password.memory_cost = {} KiB 过低（至少 {MIN_ARGON2_MEMORY_KIB} KiB，\
                 默认 19456 KiB；参见 docs/11 §6）",
                self.memory_cost
            ));
        }
        if self.memory_cost > MAX_ARGON2_MEMORY_KIB {
            return Err(format!(
                "[security].password.memory_cost = {} KiB 过高（上限 {MAX_ARGON2_MEMORY_KIB} KiB，\
                 否则每次登录都会申请巨量内存）",
                self.memory_cost
            ));
        }
        if self.time_cost < MIN_ARGON2_TIME_COST {
            return Err(format!(
                "[security].password.time_cost = {} 过低（至少 {MIN_ARGON2_TIME_COST}）",
                self.time_cost
            ));
        }
        if self.parallelism == 0 {
            return Err("[security].password.parallelism 必须大于 0".to_string());
        }
        Ok(())
    }
}

/// 密钥长度校验：留空放行，非空则必须 ≥ [`MIN_KEY_BYTES`]。
fn validate_key(field: &str, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Ok(());
    }
    if key.len() < MIN_KEY_BYTES {
        return Err(format!(
            "{field} 只有 {} 字节，低于 {MIN_KEY_BYTES} 字节（256 bit）下限；\
             对称密钥请使用 ≥32 字节随机值",
            key.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_argon2id_recommended_params() {
        let s = SecuritySection::default();
        assert_eq!(s.password.memory_cost, 19_456);
        assert_eq!(s.password.time_cost, 2);
        assert_eq!(s.password.parallelism, 1);
        assert_eq!(s.crypto.algorithm, CryptoAlgorithm::AesGcm);
        // 密钥只走环境变量，配置默认值为空
        assert!(s.crypto.key.is_empty());
        assert!(s.hmac.key.is_empty());
    }

    #[test]
    fn should_parse_documented_algorithm_names() {
        // 文档写的是 `aes-gcm`；`rename_all = "lowercase"` 只认 `aesgcm`，
        // 配错会直接启动失败（且错误信息只说「unknown variant」）
        let json = r#"{ "crypto": { "algorithm": "aes-gcm" } }"#;
        let s: SecuritySection = serde_json::from_str(json).unwrap();
        assert_eq!(s.crypto.algorithm, CryptoAlgorithm::AesGcm);

        let json = r#"{ "crypto": { "algorithm": "chacha20poly1305" } }"#;
        let s: SecuritySection = serde_json::from_str(json).unwrap();
        assert_eq!(s.crypto.algorithm, CryptoAlgorithm::Chacha20poly1305);
    }

    #[test]
    fn should_parse_argon2_params() {
        let json = r#"
            {
                "password": { "memory_cost": 8192, "time_cost": 1, "parallelism": 2 },
                "crypto": { "algorithm": "chacha20poly1305" }
            }
        "#;
        let s: SecuritySection = serde_json::from_str(json).unwrap();
        assert_eq!(s.password.memory_cost, 8192);
        assert_eq!(s.password.time_cost, 1);
        assert_eq!(s.password.parallelism, 2);
        assert_eq!(s.crypto.algorithm, CryptoAlgorithm::Chacha20poly1305);
    }

    #[test]
    fn should_allow_empty_keys() {
        // 未使用对称加密 / HMAC 时留空是合法的
        assert!(SecuritySection::default().validate().is_ok());
    }

    #[test]
    fn should_reject_short_keys() {
        let short = SecuritySection {
            crypto: CryptoSecurity {
                key: "0123456789abcdef".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        let error = short.validate().unwrap_err();
        assert!(error.contains("crypto.key"), "实际：{error}");

        let hmac = SecuritySection {
            hmac: HmacSecurity {
                key: "short".to_string(),
            },
            ..SecuritySection::default()
        };
        assert!(hmac.validate().unwrap_err().contains("hmac.key"));
    }

    #[test]
    fn should_accept_32_byte_keys() {
        let key = "0123456789abcdef0123456789abcdef";
        let s = SecuritySection {
            crypto: CryptoSecurity {
                key: key.to_string(),
                ..CryptoSecurity::default()
            },
            hmac: HmacSecurity {
                key: key.to_string(),
            },
            ..SecuritySection::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_previous_key_without_current_key() {
        let s = SecuritySection {
            crypto: CryptoSecurity {
                previous_key: "0123456789abcdef0123456789abcdef".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        let error = s.validate().unwrap_err();
        assert!(error.contains("previous_key"), "实际：{error}");
    }

    #[test]
    fn should_reject_short_previous_key() {
        let s = SecuritySection {
            crypto: CryptoSecurity {
                key: "0123456789abcdef0123456789abcdef".to_string(),
                previous_key: "short".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(s.validate().unwrap_err().contains("previous_key"));
    }

    #[test]
    fn should_accept_rotation_pair() {
        let s = SecuritySection {
            crypto: CryptoSecurity {
                key: "0123456789abcdef0123456789abcdef".to_string(),
                previous_key: "fedcba9876543210fedcba9876543210".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_weak_argon2_params() {
        let weak = SecuritySection {
            password: PasswordSecurity {
                memory_cost: 1024,
                ..PasswordSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(weak.validate().is_err(), "内存开销过低应被拒绝");

        let fast = SecuritySection {
            password: PasswordSecurity {
                time_cost: 1,
                ..PasswordSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(fast.validate().is_err(), "迭代次数过低应被拒绝");

        let zero_parallelism = SecuritySection {
            password: PasswordSecurity {
                parallelism: 0,
                ..PasswordSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(zero_parallelism.validate().is_err());
    }
}
