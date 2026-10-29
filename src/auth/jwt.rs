//! JWT 签发与校验。
//!
//! - 算法：**单体 / 单服务 `HS256`；多服务验签 `RS256`**（见 `docs/11` §2.1）
//! - 双令牌：短 `access` + 长 `refresh`，`Claims.token_type` 区分，防止 refresh 当 access 用
//! - `jti` 用于黑名单（`docs/11` §2.2，依赖 `cache-redis`）
//! - 校验签名 + `exp` + `iss` + `token_type`；失败 → `20001` / `20003`（401）
//! - **`aud`（受众）**：`[auth].jwt.audience` 非空时强制校验。多应用共用同一套
//!   密钥/issuer 时，没有 `aud` 等于"任何应用签发的令牌都能打任何应用"。

use std::time::Duration;

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

use crate::auth::identity::Principal;
use crate::config::sections::auth::{JwtAlgorithm, JwtSection};
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::utils::Secret;

/// access 令牌的 `token_type`。
pub const ACCESS_TOKEN: &str = "access";
/// refresh 令牌的 `token_type`。
pub const REFRESH_TOKEN: &str = "refresh";
/// `Authorization` 头的 Bearer 前缀。
pub const BEARER_PREFIX: &str = "Bearer ";

/// JWT 载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// 主体标识。
    pub sub: String,
    /// 过期时刻（Unix 秒）。
    pub exp: usize,
    /// 签发时刻（Unix 秒）。
    pub iat: usize,
    /// 签发者。
    pub iss: String,
    /// 受众（多应用隔离）。
    ///
    /// `[auth].jwt.audience` 为空时本字段不参与校验（兼容历史令牌）；
    /// 非空时令牌**必须**携带且命中其一。
    #[serde(default, deserialize_with = "de_audience")]
    pub aud: Vec<String>,
    /// 令牌唯一 id（用于黑名单）。
    pub jti: String,
    /// 刷新令牌家族 id（轮换 + 重用检测，见 [`RefreshRotation`](crate::auth::RefreshRotation)）。
    ///
    /// 同一次登录派生出的所有 refresh 令牌共享一个 `fid`：每次刷新都换新的
    /// `jti`、但沿用同一个 `fid`。一旦某条 refresh 被**第二次**使用，说明它
    /// 已经泄漏（合法用户手上那份已经换走了），整条家族一起拉黑。
    ///
    /// 旧令牌（签发于本字段引入之前）为空串，此时按 `sub` 归族——宁可多吊销
    /// 一个用户的全部会话，也不要漏掉一次泄漏。
    #[serde(default)]
    pub fid: String,
    /// 令牌类型（`access` / `refresh`）。
    pub token_type: String,
    /// 角色。
    pub roles: Vec<String>,
    /// 细粒度权限。
    pub perms: Vec<String>,
}

impl Claims {
    /// 由 [`Principal`] 派生主体信息。
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal {
            sub: self.sub.clone(),
            roles: self.roles.clone(),
            perms: self.perms.clone(),
        }
    }

    /// 是否为 access 令牌。
    #[must_use]
    pub fn is_access(&self) -> bool {
        self.token_type == ACCESS_TOKEN
    }

    /// 是否为 refresh 令牌。
    #[must_use]
    pub fn is_refresh(&self) -> bool {
        self.token_type == REFRESH_TOKEN
    }
}

/// 签发结果（access + refresh 双令牌）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenPair {
    /// 访问令牌。
    pub access_token: String,
    /// 刷新令牌。
    pub refresh_token: String,
    /// 令牌类型（固定 `Bearer`）。
    pub token_type: &'static str,
    /// access 令牌有效期（秒）。
    pub expires_in: i64,
}

/// JWT 编解码器。
///
/// 以 `Arc` 共享（`AuthService.jwt`）；密钥只在签发时校验（惰性必填）。
#[derive(Clone)]
pub struct JwtCodec {
    /// JWT 密钥。用 [`Secret`] 包一层：`Debug` 掩码、`Drop` 清零。
    /// 这是全框架权限最高的密钥，不该是裸 `String`。
    secret: Secret<String>,
    algorithm: JwtAlgorithm,
    access_ttl: Duration,
    refresh_ttl: Duration,
    issuer: String,
    /// 受众；空表示不校验。
    audience: Vec<String>,
}

impl JwtCodec {
    /// 由 `[auth].jwt` 配置构造。
    #[must_use]
    pub fn new(config: &JwtSection) -> Self {
        Self {
            secret: Secret::new(config.secret.clone()),
            algorithm: config.algorithm,
            access_ttl: config.access_ttl,
            refresh_ttl: config.refresh_ttl,
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
        }
    }

    /// access 令牌有效期。
    #[must_use]
    pub fn access_ttl(&self) -> Duration {
        self.access_ttl
    }

    /// refresh 令牌有效期。
    #[must_use]
    pub fn refresh_ttl(&self) -> Duration {
        self.refresh_ttl
    }

    /// 签发 access + refresh 双令牌。
    ///
    /// # Errors
    /// 密钥为空（`20001`）或底层编码失败（`50001`）时返回错误。
    pub fn sign_token_pair(&self, principal: &Principal) -> AppResult<TokenPair> {
        self.sign_token_pair_with_family(principal, &new_jti())
    }

    /// 签发一对令牌，并指定刷新令牌家族 id。
    ///
    /// 轮换时用旧 refresh 的 `fid` 调这个，让新令牌留在同一家族里——
    /// 家族内出现「同一条 refresh 用两次」才能被判定为泄漏。
    ///
    /// # Errors
    /// 密钥为空时返回 `20001`；编码失败返回 `50001`。
    pub fn sign_token_pair_with_family(
        &self,
        principal: &Principal,
        fid: &str,
    ) -> AppResult<TokenPair> {
        let access = self.sign_with_family(principal, ACCESS_TOKEN, fid)?;
        let refresh = self.sign_with_family(principal, REFRESH_TOKEN, fid)?;
        Ok(TokenPair {
            access_token: access,
            refresh_token: refresh,
            token_type: "Bearer",
            expires_in: self.access_ttl.as_secs() as i64,
        })
    }

    /// 签发指定类型的令牌。
    ///
    /// # Errors
    /// 密钥为空时返回 `20001`；编码失败返回 `50001`。
    pub fn sign(&self, principal: &Principal, token_type: &str) -> AppResult<String> {
        self.sign_with_family(principal, token_type, "")
    }

    /// 签发指定类型、指定家族 id 的令牌。
    ///
    /// `fid` 为空表示「不属于任何家族」（轮换关闭时的行为）。
    ///
    /// # Errors
    /// 密钥为空时返回 `20001`；编码失败返回 `50001`。
    pub fn sign_with_family(
        &self,
        principal: &Principal,
        token_type: &str,
        fid: &str,
    ) -> AppResult<String> {
        self.require_secret()?;
        let now = now_secs();
        let ttl = match token_type {
            ACCESS_TOKEN => self.access_ttl,
            _ => self.refresh_ttl,
        };
        let claims = Claims {
            sub: principal.sub.clone(),
            exp: now + ttl.as_secs() as usize,
            iat: now,
            iss: self.issuer.clone(),
            jti: new_jti(),
            fid: fid.to_string(),
            token_type: token_type.to_string(),
            aud: self.audience.clone(),
            roles: principal.roles.clone(),
            perms: principal.perms.clone(),
        };
        encode(&self.header(), &claims, &self.encoding_key()?)
            .map_err(|e| AppError::internal(anyhow::anyhow!("jwt encode failed: {e}")))
    }

    /// 校验令牌（不限类型）。
    ///
    /// # Errors
    /// 过期 → `20003`；签名/格式/签发者不符 → `20001`。
    pub fn verify(&self, token: &str) -> AppResult<Claims> {
        self.verify_with_type(token, None)
    }

    /// 校验 access 令牌（拒绝 refresh）。
    ///
    /// # Errors
    /// 见 [`Self::verify`]；类型不符额外报 `20003`。
    pub fn verify_access(&self, token: &str) -> AppResult<Claims> {
        self.verify_with_type(token, Some(ACCESS_TOKEN))
    }

    /// 校验 refresh 令牌（拒绝 access）。
    ///
    /// # Errors
    /// 见 [`Self::verify`]；类型不符额外报 `20003`。
    pub fn verify_refresh(&self, token: &str) -> AppResult<Claims> {
        self.verify_with_type(token, Some(REFRESH_TOKEN))
    }

    /// 从 `Authorization: Bearer <token>` 头中取出 token。
    ///
    /// 方案名**大小写不敏感**（RFC 7235 §2.1）：`bearer` / `BEARER` 都是合法写法，
    /// 按字面量比较会把某些 SDK / 网关挡在门外。
    #[must_use]
    pub fn bearer_token(header_value: &str) -> Option<&str> {
        let (scheme, token) = header_value.trim().split_once(' ')?;
        scheme
            .eq_ignore_ascii_case(BEARER_PREFIX.trim_end())
            .then(|| token.trim())
            .filter(|token| !token.is_empty())
    }

    fn verify_with_type(&self, token: &str, expected: Option<&str>) -> AppResult<Claims> {
        let data = decode::<Claims>(token, &self.decoding_key()?, &self.validation())
            .map_err(map_decode_error)?;

        if let Some(expected) = expected
            && data.claims.token_type != expected
        {
            return Err(AppError::biz(
                ErrorCode::TOKEN_EXPIRED,
                format!("expected a {expected} token"),
            ));
        }
        Ok(data.claims)
    }

    fn require_secret(&self) -> AppResult<()> {
        if self.secret.expose().trim().is_empty() {
            return Err(AppError::internal(anyhow::anyhow!(
                "auth.jwt.secret is required for signing (set AURORA_AUTH__JWT__SECRET)"
            )));
        }
        Ok(())
    }

    fn header(&self) -> Header {
        Header::new(self.algorithm.into())
    }

    fn encoding_key(&self) -> AppResult<EncodingKey> {
        match self.algorithm {
            JwtAlgorithm::HS256 => Ok(EncodingKey::from_secret(self.secret.expose().as_bytes())),
            JwtAlgorithm::RS256 => EncodingKey::from_rsa_pem(self.secret.expose().as_bytes())
                .map_err(|e| AppError::internal(anyhow::anyhow!("invalid RSA private key: {e}"))),
        }
    }

    fn decoding_key(&self) -> AppResult<DecodingKey> {
        match self.algorithm {
            JwtAlgorithm::HS256 => Ok(DecodingKey::from_secret(self.secret.expose().as_bytes())),
            JwtAlgorithm::RS256 => DecodingKey::from_rsa_pem(self.secret.expose().as_bytes())
                .map_err(|e| AppError::internal(anyhow::anyhow!("invalid RSA public key: {e}"))),
        }
    }

    fn validation(&self) -> Validation {
        let mut validation = Validation::new(self.algorithm.into());
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.validate_exp = true;
        validation.leeway = 0;

        if self.audience.is_empty() {
            // 未配置受众：不校验（兼容历史令牌）
            validation.validate_aud = false;
        } else {
            validation.set_audience(&self.audience);
            validation.validate_aud = true;
            // **关键**：jsonwebtoken 只在令牌**带** `aud` 时才校验；不加进
            // required_spec_claims 的话，一个不带 aud 的旧令牌照样通过——
            // 那就等于没配。
            validation.required_spec_claims.insert("aud".to_string());
        }
        validation
    }
}

impl From<JwtAlgorithm> for Algorithm {
    fn from(algorithm: JwtAlgorithm) -> Self {
        match algorithm {
            JwtAlgorithm::HS256 => Self::HS256,
            JwtAlgorithm::RS256 => Self::RS256,
        }
    }
}

/// 反序列化 `aud`：同时接受 `"a"` 与 `["a","b"]`（RFC 7519 §4.1.3 允许两者）。
fn de_audience<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// 单值或数组。
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        /// 单个受众。
        One(String),
        /// 多个受众。
        Many(Vec<String>),
    }

    Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(OneOrMany::One(aud)) => vec![aud],
        Some(OneOrMany::Many(list)) => list,
    })
}

/// 当前 Unix 秒。
fn now_secs() -> usize {
    crate::utils::now_unix_secs().max(0) as usize
}

/// 生成令牌唯一 id（32 位十六进制）。
fn new_jti() -> String {
    crate::telemetry::new_trace_id()
}

/// 将 `jsonwebtoken::errors::Error` 映射为 [`AppError`]。
fn map_decode_error(err: jsonwebtoken::errors::Error) -> AppError {
    use jsonwebtoken::errors::ErrorKind;

    match err.kind() {
        ErrorKind::ExpiredSignature => AppError::biz(ErrorCode::TOKEN_EXPIRED, "token expired"),
        ErrorKind::ImmatureSignature => {
            AppError::biz(ErrorCode::TOKEN_EXPIRED, "token not yet valid")
        }
        _ => {
            tracing::debug!(error = %err, "jwt verification failed");
            AppError::biz(ErrorCode::UNAUTHENTICATED, "invalid token")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec() -> JwtCodec {
        JwtCodec::new(&JwtSection {
            secret: "test-secret".to_string(),
            algorithm: JwtAlgorithm::HS256,
            access_ttl: Duration::from_secs(60),
            refresh_ttl: Duration::from_secs(3600),
            issuer: "aurora".to_string(),
            audience: Vec::new(),
        })
    }

    /// 带受众的编解码器。
    fn codec_with_audience(audience: &[&str]) -> JwtCodec {
        JwtCodec::new(&JwtSection {
            secret: "test-secret".to_string(),
            algorithm: JwtAlgorithm::HS256,
            access_ttl: Duration::from_secs(60),
            refresh_ttl: Duration::from_secs(3600),
            issuer: "aurora".to_string(),
            audience: audience.iter().map(|a| (*a).to_string()).collect(),
        })
    }

    #[test]
    fn should_roundtrip_token_pair() {
        let codec = codec();
        let principal = Principal::from_id(7).with_role("admin");
        let pair = codec.sign_token_pair(&principal).unwrap();

        assert_eq!(pair.token_type, "Bearer");
        assert_eq!(pair.expires_in, 60);

        let access = codec.verify_access(&pair.access_token).unwrap();
        assert_eq!(access.sub, "7");
        assert!(access.is_access());
        assert_eq!(access.principal(), principal);
        assert_ne!(access.jti.len(), 0);

        let refresh = codec.verify_refresh(&pair.refresh_token).unwrap();
        assert!(refresh.is_refresh());
    }

    #[test]
    fn should_reject_refresh_used_as_access() {
        let codec = codec();
        let pair = codec.sign_token_pair(&Principal::new("u")).unwrap();
        let err = codec.verify_access(&pair.refresh_token).unwrap_err();
        assert_eq!(err.code(), ErrorCode::TOKEN_EXPIRED);
    }

    #[test]
    fn should_reject_wrong_issuer() {
        let codec = codec();
        let token = codec.sign(&Principal::new("u"), ACCESS_TOKEN).unwrap();

        let other = JwtCodec::new(&JwtSection {
            secret: "test-secret".to_string(),
            issuer: "other".to_string(),
            ..JwtSection::default()
        });
        let err = other.verify(&token).unwrap_err();
        assert_eq!(err.code(), ErrorCode::UNAUTHENTICATED);
    }

    #[test]
    fn should_reject_tampered_token() {
        let codec = codec();
        let token = codec.sign(&Principal::new("u"), ACCESS_TOKEN).unwrap();
        let mut tampered = token.clone();
        tampered.push('x');
        let err = codec.verify(&tampered).unwrap_err();
        assert_eq!(err.code(), ErrorCode::UNAUTHENTICATED);
    }

    #[test]
    fn should_reject_expired_token() {
        let codec = JwtCodec::new(&JwtSection {
            secret: "test-secret".to_string(),
            access_ttl: Duration::from_secs(0),
            ..JwtSection::default()
        });
        let token = codec.sign(&Principal::new("u"), ACCESS_TOKEN).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        let err = codec.verify_access(&token).unwrap_err();
        assert_eq!(err.code(), ErrorCode::TOKEN_EXPIRED);
    }

    #[test]
    fn should_fail_when_secret_empty() {
        let codec = JwtCodec::new(&JwtSection::default());
        let err = codec.sign(&Principal::new("u"), ACCESS_TOKEN).unwrap_err();
        assert_eq!(err.code(), ErrorCode::INTERNAL);
    }

    #[test]
    fn should_extract_bearer_token() {
        assert_eq!(JwtCodec::bearer_token("Bearer abc"), Some("abc"));
        // RFC 7235 §2.1：方案名大小写不敏感
        assert_eq!(JwtCodec::bearer_token("bearer abc"), Some("abc"));
        assert_eq!(JwtCodec::bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(JwtCodec::bearer_token("  Bearer   abc  "), Some("abc"));
        assert_eq!(JwtCodec::bearer_token("Basic abc"), None);
        assert_eq!(JwtCodec::bearer_token("Bearer "), None);
        assert_eq!(JwtCodec::bearer_token("Bearer"), None);
    }

    #[test]
    fn should_sign_and_verify_audience() {
        let codec = codec_with_audience(&["app-a"]);
        let token = codec.sign(&Principal::new("u"), ACCESS_TOKEN).unwrap();

        // 同受众：通过
        assert!(codec.verify_access(&token).is_ok());
        // 多受众配置里命中其一即通过
        let multi = codec_with_audience(&["app-a", "app-b"]);
        assert!(multi.verify_access(&token).is_ok());
        // 别的应用：拒绝
        let other = codec_with_audience(&["app-b"]);
        let error = other.verify_access(&token).unwrap_err();
        assert_eq!(error.code(), ErrorCode::UNAUTHENTICATED);
    }

    #[test]
    fn should_reject_token_without_audience_when_configured() {
        // 未配置 aud 时签出的令牌（claims.aud 为空）
        let no_aud = codec().sign(&Principal::new("u"), ACCESS_TOKEN).unwrap();

        // 目标应用配置了 aud → 必须拒绝"没有 aud 的旧令牌"，
        // 否则 required_spec_claims 缺 "aud" 会让校验形同虚设
        let enforcing = codec_with_audience(&["app-a"]);
        let error = enforcing.verify_access(&no_aud).unwrap_err();
        assert_eq!(error.code(), ErrorCode::UNAUTHENTICATED);
    }

    #[test]
    fn should_accept_string_or_array_audience() {
        // RFC 7519 允许 aud 是字符串或数组
        let json = serde_json::json!({ "aud": "app-a" });
        let parsed: Result<Vec<String>, _> = de_audience_standalone(json);
        assert_eq!(parsed.unwrap(), vec!["app-a".to_string()]);

        let json = serde_json::json!({ "aud": ["app-a", "app-b"] });
        let parsed: Result<Vec<String>, _> = de_audience_standalone(json);
        assert_eq!(
            parsed.unwrap(),
            vec!["app-a".to_string(), "app-b".to_string()]
        );
    }

    /// 用 `serde_json` 驱动 `de_audience`（避免手写 Deserializer）。
    fn de_audience_standalone(value: serde_json::Value) -> Result<Vec<String>, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "de_audience")]
            aud: Vec<String>,
        }
        serde_json::from_value::<Holder>(value).map(|holder| holder.aud)
    }
}
