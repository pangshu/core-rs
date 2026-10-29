//! `[auth]` / `[authz]` 配置段（feature `auth`）。

use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::de::de_duration;

/// HS256 密钥的最小字节数（256 bit）。
pub const MIN_JWT_SECRET_BYTES: usize = 32;

/// 认证配置段。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct AuthSection {
    /// JWT 配置。
    pub jwt: JwtSection,
    /// 会话配置（feature `session`）。
    pub session: SessionSection,
    /// jti 黑名单（feature `revocation`）。
    pub revocation: RevocationSection,
}

/// jti 黑名单配置（feature `revocation`）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct RevocationSection {
    /// 是否在鉴权时校验 jti 黑名单（默认关闭：每个请求多一次缓存读）。
    pub enabled: bool,
    /// 吊销存储不可用时是否**拒绝**请求（默认 `false`：按未吊销处理）。
    ///
    /// 打开后 Redis 故障会让所有已认证请求返回 `50005`；资金类系统建议打开。
    pub fail_closed: bool,
}

/// JWT 配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct JwtSection {
    /// JWT 密钥（敏感项 🔒，只走环境变量 `AURORA_AUTH__JWT__SECRET`；
    /// **惰性必填**——仅在实际签发 token 时校验）。
    pub secret: String,
    /// 签名算法（单体 / 单服务 `HS256`，多服务验签 `RS256`）。
    pub algorithm: JwtAlgorithm,
    /// 访问令牌有效期。
    #[serde(deserialize_with = "de_duration")]
    pub access_ttl: Duration,
    /// 刷新令牌有效期。
    #[serde(deserialize_with = "de_duration")]
    pub refresh_ttl: Duration,
    /// 签发者。
    pub issuer: String,
    /// 受众（`aud`）。**多应用共用同一套密钥时必须配置**，否则 A 应用的
    /// token 可以直接打 B 应用（同一 issuer、同一密钥，B 无从区分）。
    ///
    /// 空数组 = 不校验 `aud`（向后兼容）；非空 = 令牌**必须**带 `aud` 且命中其一。
    pub audience: Vec<String>,
}

impl JwtSection {
    /// 校验密钥与有效期。
    ///
    /// `production = true` 时密钥**必填**且 HS256 需 ≥32 字节；非生产环境
    /// 只告警（便于本地起服务时不配密钥）。空密钥在验签侧等价于「任何人都能
    /// 伪造 token」，因此生产必须 fail-fast。
    ///
    /// # Errors
    /// 生产环境密钥缺失 / 过短，或有效期非法时返回错误。
    pub fn validate(&self, production: bool) -> Result<(), String> {
        if self.access_ttl.is_zero() {
            return Err("[auth].jwt.access_ttl 必须大于 0".to_string());
        }
        if self.refresh_ttl.is_zero() {
            return Err("[auth].jwt.refresh_ttl 必须大于 0".to_string());
        }
        if self.refresh_ttl < self.access_ttl {
            return Err("[auth].jwt.refresh_ttl 不能小于 access_ttl".to_string());
        }
        if self.issuer.trim().is_empty() {
            return Err("[auth].jwt.issuer 不能为空（验签会按 issuer 校验）".to_string());
        }
        // `aud`：空数组表示不校验；一旦配置就必须是非空字符串
        if self.audience.iter().any(|aud| aud.trim().is_empty()) {
            return Err("[auth].jwt.audience 不能包含空字符串".to_string());
        }

        let secret = self.secret.trim();
        if secret.is_empty() {
            if production {
                return Err(
                    "[auth].jwt.secret 在生产环境必填（AURORA_AUTH__JWT__SECRET）；\
                     空密钥意味着任何人都能伪造合法 token"
                        .to_string(),
                );
            }
            tracing::warn!(
                "[auth].jwt.secret 未设置：签发 token 会失败，验签等同于不校验，仅可用于本地开发"
            );
            return Ok(());
        }
        if self.algorithm == JwtAlgorithm::HS256 && secret.len() < MIN_JWT_SECRET_BYTES {
            return Err(format!(
                "[auth].jwt.secret 只有 {} 字节，HS256 要求至少 {MIN_JWT_SECRET_BYTES} 字节（256 bit）",
                secret.len()
            ));
        }
        Ok(())
    }
}

impl Default for JwtSection {
    fn default() -> Self {
        Self {
            secret: String::new(),
            algorithm: JwtAlgorithm::HS256,
            access_ttl: Duration::from_secs(15 * 60),
            refresh_ttl: Duration::from_secs(7 * 24 * 3600),
            issuer: "aurora".to_string(),
            audience: Vec::new(),
        }
    }
}

/// JWT 签名算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
#[non_exhaustive]
pub enum JwtAlgorithm {
    /// HMAC-SHA256（单体 / 单服务）。
    #[default]
    HS256,
    /// RSA PKCS#1 v1.5 + SHA-256（多服务验签）。
    RS256,
}

/// 会话配置段（feature `session`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct SessionSection {
    /// 是否启用会话认证。
    pub enabled: bool,
    /// 会话 Cookie 名。
    pub cookie_name: String,
    /// 会话有效期。
    #[serde(deserialize_with = "de_duration")]
    pub ttl: Duration,
    /// 是否仅 HTTPS 传输。
    pub secure: bool,
    /// `SameSite` 属性。
    pub same_site: SameSite,
    /// 存储后端。
    pub store: SessionStoreKind,
}

impl Default for SessionSection {
    fn default() -> Self {
        Self {
            enabled: false,
            cookie_name: "sid".to_string(),
            ttl: Duration::from_secs(24 * 3600),
            secure: true,
            same_site: SameSite::Lax,
            store: SessionStoreKind::Memory,
        }
    }
}

impl AuthSection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// 会话 Cookie 名 / TTL 非法，或 `store = redis` 但未启用 `session-redis` 时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// `production = true` 时额外强制：
    /// - `jwt.secret` 必填且（HS256）≥32 字节；
    /// - 启用会话时 Cookie 必须 `secure`（`same_site = none` 尤其如此）；
    /// - 启用吊销但 `fail_closed = false` 时告警（Redis 故障会放行已吊销 token）。
    ///
    /// # Errors
    /// 上述任一条不满足时返回错误。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        self.jwt.validate(production)?;
        self.session.validate()?;

        if self.session.enabled && !self.session.secure {
            if production {
                return Err(
                    "[auth].session.secure = false 在生产环境被拒绝：会话 Cookie 会以明文 HTTP 传输"
                        .to_string(),
                );
            }
            tracing::warn!("[auth].session.secure = false：会话 Cookie 可被明文 HTTP 截获");
        }
        if self.session.enabled
            && self.session.same_site == SameSite::None
            && !self.session.secure
            && production
        {
            return Err(
                "[auth].session.same_site = \"none\" 必须配合 secure = true（生产环境）"
                    .to_string(),
            );
        }
        if self.revocation.enabled && !self.revocation.fail_closed && production {
            tracing::warn!(
                "[auth].revocation.fail_closed = false：吊销存储（Redis）故障时会放行已吊销的 token"
            );
        }
        if self.revocation.enabled && !cfg!(feature = "revocation") {
            return Err(
                "[auth].revocation.enabled = true 需要启用 `revocation` feature\
                 （并确保 [cache] 可用）"
                    .to_string(),
            );
        }
        Ok(())
    }
}

impl SessionSection {
    /// 启动期校验（fail-safe：坏配置在启动时就报，而不是等到第一次登录）。
    ///
    /// # Errors
    /// Cookie 名为空 / 含非法字符、TTL 为 0、或 `store = redis` 但缺少 feature。
    pub fn validate(&self) -> Result<(), String> {
        if self.cookie_name.trim().is_empty() {
            return Err("auth.session.cookie_name 不能为空".to_string());
        }
        if !self
            .cookie_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(format!(
                "auth.session.cookie_name = \"{}\" 含非法字符（只允许字母数字、`_`、`-`）",
                self.cookie_name
            ));
        }
        if self.ttl.is_zero() {
            return Err("auth.session.ttl 必须大于 0".to_string());
        }
        if self.store == SessionStoreKind::Redis && !cfg!(feature = "session-redis") {
            return Err("auth.session.store = redis 需要启用 `session-redis` feature".to_string());
        }
        // `__Host-` 前缀由浏览器强制「Secure + Path=/ + 无 Domain」，
        // 能挡住兄弟子域覆盖会话 Cookie（cookie tossing）。
        let name = self.cookie_name.trim();
        if name.starts_with("__Host-") && !self.secure {
            return Err(
                "auth.session.cookie_name 使用 `__Host-` 前缀时必须 secure = true                 （否则浏览器会拒绝该 Cookie，登录态永远建立不起来）"
                    .to_string(),
            );
        }
        if !name.starts_with("__Host-") {
            tracing::warn!(
                cookie = %name,
                "auth.session.cookie_name 未使用 `__Host-` 前缀：建议改为 __Host-{}（需 secure = true），                 以阻止兄弟子域覆盖会话 Cookie",
                name
            );
        }
        Ok(())
    }
}

/// Cookie `SameSite` 属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SameSite {
    /// `Strict`。
    Strict,
    /// `Lax`（默认）。
    Lax,
    /// `None`（跨站，需配合 CSRF）。
    None,
}

/// 会话存储后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SessionStoreKind {
    /// 单实例内存。
    Memory,
    /// 多实例 Redis。
    Redis,
}

/// 授权配置段（Casbin RBAC，feature `authz`）。
///
/// 只有启用 `authz` 时该段才存在：否则配置文件里写 `[authz]` 会被
/// `deny_unknown_fields` 拒绝——「配了但没编译」必须报错，不能静默忽略。
#[cfg(feature = "authz")]
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct AuthzSection {
    /// 模型文件路径。
    pub model_path: String,
    /// 策略文件路径。
    pub policy_path: String,
    /// 适配器。
    pub adapter: AuthzAdapter,
}

#[cfg(feature = "authz")]
impl Default for AuthzSection {
    fn default() -> Self {
        Self {
            model_path: "config/rbac_model.conf".to_string(),
            policy_path: "config/rbac_policy.csv".to_string(),
            adapter: AuthzAdapter::File,
        }
    }
}

/// 授权适配器。
#[cfg(feature = "authz")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AuthzAdapter {
    /// 文件适配器。
    File,
    /// 数据库适配器。
    Db,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_hs256_and_short_access() {
        let s = AuthSection::default();
        assert_eq!(s.jwt.algorithm, JwtAlgorithm::HS256);
        assert_eq!(s.jwt.access_ttl, Duration::from_secs(15 * 60));
        assert_eq!(s.jwt.refresh_ttl, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(s.jwt.issuer, "aurora");
        // 密钥惰性必填，配置默认值为空
        assert!(s.jwt.secret.is_empty());
        assert!(!s.session.enabled);
        assert_eq!(s.session.same_site, SameSite::Lax);
    }

    #[test]
    fn should_parse_jwt_and_session_options() {
        let json = r#"
            {
                "jwt": {
                    "algorithm": "RS256",
                    "access_ttl": "30m",
                    "refresh_ttl": "14d",
                    "issuer": "api.example.com"
                },
                "session": { "enabled": true, "cookie_name": "sid", "same_site": "none", "store": "redis" }
            }
        "#;
        let s: AuthSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.jwt.algorithm, JwtAlgorithm::RS256);
        assert_eq!(s.jwt.access_ttl, Duration::from_secs(30 * 60));
        assert_eq!(s.jwt.refresh_ttl, Duration::from_secs(14 * 24 * 3600));
        assert_eq!(s.jwt.issuer, "api.example.com");
        assert!(s.session.enabled);
        assert_eq!(s.session.same_site, SameSite::None);
        assert_eq!(s.session.store, SessionStoreKind::Redis);
    }

    #[cfg(feature = "authz")]
    #[test]
    fn should_default_authz_to_file_adapter() {
        let s = AuthzSection::default();
        assert_eq!(s.adapter, AuthzAdapter::File);
        assert_eq!(s.model_path, "config/rbac_model.conf");
    }

    #[test]
    fn should_validate_default_session() {
        assert!(SessionSection::default().validate().is_ok());
    }

    #[test]
    fn should_reject_empty_cookie_name() {
        let s = SessionSection {
            cookie_name: String::new(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_illegal_cookie_name() {
        let s = SessionSection {
            cookie_name: "bad name".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_zero_ttl() {
        let s = SessionSection {
            ttl: Duration::ZERO,
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_require_session_redis_feature_for_redis_store() {
        let s = SessionSection {
            store: SessionStoreKind::Redis,
            ..Default::default()
        };
        if cfg!(feature = "session-redis") {
            assert!(s.validate().is_ok(), "启用 session-redis 后应通过校验");
        } else {
            let error = s.validate().unwrap_err();
            assert!(error.contains("session-redis"), "实际：{error}");
        }
    }

    #[test]
    fn should_require_jwt_secret_in_production() {
        let s = AuthSection::default();
        // 非生产：空密钥只告警
        assert!(s.validate_for_env(false).is_ok());
        // 生产：空密钥必须拒绝
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("jwt.secret"), "实际：{error}");
    }

    #[test]
    fn should_reject_short_hs256_secret() {
        let s = AuthSection {
            jwt: JwtSection {
                secret: "too-short".to_string(),
                ..JwtSection::default()
            },
            ..AuthSection::default()
        };
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("HS256"), "实际：{error}");

        let ok = AuthSection {
            jwt: JwtSection {
                secret: "0123456789abcdef0123456789abcdef".to_string(),
                ..JwtSection::default()
            },
            ..AuthSection::default()
        };
        assert!(ok.validate_for_env(true).is_ok());
    }

    #[test]
    fn should_reject_insecure_session_in_production() {
        let s = AuthSection {
            jwt: JwtSection {
                secret: "0123456789abcdef0123456789abcdef".to_string(),
                ..JwtSection::default()
            },
            session: SessionSection {
                enabled: true,
                secure: false,
                ..SessionSection::default()
            },
            ..AuthSection::default()
        };
        assert!(s.validate_for_env(false).is_ok(), "非生产只告警");
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("session.secure"), "实际：{error}");
    }

    #[test]
    fn should_reject_zero_or_inverted_ttl() {
        let zero = AuthSection {
            jwt: JwtSection {
                access_ttl: Duration::ZERO,
                ..JwtSection::default()
            },
            ..AuthSection::default()
        };
        assert!(zero.validate().is_err());

        let inverted = AuthSection {
            jwt: JwtSection {
                access_ttl: Duration::from_secs(3600),
                refresh_ttl: Duration::from_secs(60),
                ..JwtSection::default()
            },
            ..AuthSection::default()
        };
        assert!(inverted.validate().is_err(), "refresh < access 应被拒绝");
    }
}
