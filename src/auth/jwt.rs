//! JWT 签发与校验（feature = "jwt"，HS256，文档 三·13）。
//!
//! - 多 audience：用户端 / 管理端各自 Claims（`[auth.jwt].audiences` 配置后校验 aud）；
//! - 刷新机制：token 携带首次签发时间 `orig_iat`，在 `[auth.jwt].max_refresh_hours`
//!   预算内允许过期 token 换新，新 token 保留原 `orig_iat`（续期不重置起点，
//!   token 无法无限续期）；过期响应为 HTTP 401 + 响应体 `code = 6401`；
//! - [`JwtAuthn`] 把它接入统一 [`Authn`](super::Authn) 链（Bearer 头识别）。

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use axum::http::request::Parts;

use crate::config::sections::JwtSettings;
use crate::error::{AppError, AppResult};

use super::{Authn, Identity};

/// 标准 Claims；`sign_with` 可携带自定义字段（读取端 `verify_value` 拿完整 JSON）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// 用户标识
    pub sub: String,
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
    /// 首次签发时间（秒）；刷新不重置。旧版本 token 无此字段时为 0（不可刷新）
    #[serde(default)]
    pub orig_iat: i64,
    /// 角色列表（`sign_with_roles` / `sign_with` 携带；旧 token 无此字段时为空）
    #[serde(default)]
    pub roles: Vec<String>,
    /// audience（多端隔离）；未配置 audiences 时不签发/不校验
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Jwt {
    encode_key: EncodingKey,
    decode_key: DecodingKey,
    issuer: String,
    expire_hours: i64,
    max_refresh_secs: i64,
    audiences: Vec<String>,
}

impl Jwt {
    pub fn new(settings: &JwtSettings) -> AppResult<Self> {
        if settings.secret.is_empty() {
            return Err(AppError::internal(
                "auth.jwt.secret 为空：请配置后再启用 JWT 认证（secret 只走环境变量）",
            ));
        }
        // HS256 的 secret 是唯一防线：<32 字节可被离线爆破，签发即等于授权开放
        if settings.secret.len() < 32 {
            return Err(AppError::internal(
                "auth.jwt.secret 过短：至少 32 字节（建议 openssl rand -base64 48）",
            ));
        }
        Ok(Self {
            encode_key: EncodingKey::from_secret(settings.secret.as_bytes()),
            decode_key: DecodingKey::from_secret(settings.secret.as_bytes()),
            issuer: settings.issuer.clone(),
            expire_hours: settings.expire_hours as i64,
            max_refresh_secs: settings.max_refresh_hours as i64 * 3600,
            audiences: settings.audiences.clone(),
        })
    }

    fn base_claims(&self, sub: &str, roles: Vec<String>, aud: Option<String>) -> Claims {
        let now = Utc::now().timestamp();
        Claims {
            sub: sub.to_string(),
            iss: self.issuer.clone(),
            iat: now,
            exp: now + Duration::hours(self.expire_hours).num_seconds(),
            orig_iat: now,
            roles,
            aud: aud.filter(|_| !self.audiences.is_empty()),
        }
    }

    fn encode_value(&self, value: &serde_json::Value) -> AppResult<String> {
        encode(&Header::new(Algorithm::HS256), value, &self.encode_key)
            .map_err(|e| AppError::internal(format!("jwt sign failed: {e}")))
    }

    fn encode_claims(&self, claims: &Claims) -> AppResult<String> {
        let value = serde_json::to_value(claims)
            .map_err(|e| AppError::internal(format!("jwt claims serialize failed: {e}")))?;
        self.encode_value(&value)
    }

    /// 校验 audience 是否在配置列表（未配置 audiences 时跳过）
    fn check_aud(&self, aud: &Option<String>) -> AppResult<()> {
        if self.audiences.is_empty() {
            return Ok(());
        }
        match aud {
            Some(a) if self.audiences.iter().any(|x| x == a) => Ok(()),
            other => Err(AppError::unauthorized(format!(
                "invalid audience {other:?}"
            ))),
        }
    }

    /// 为指定用户签发 token（无角色、无 audience）
    pub fn sign(&self, sub: &str) -> AppResult<String> {
        self.encode_claims(&self.base_claims(sub, Vec::new(), None))
    }

    /// 签发 token 并携带角色列表
    pub fn sign_with_roles(&self, sub: &str, roles: Vec<String>) -> AppResult<String> {
        self.encode_claims(&self.base_claims(sub, roles, None))
    }

    /// 签发 token 指定 audience（多端隔离：用户端 / 管理端各自 token）
    pub fn sign_for_audience(&self, sub: &str, roles: Vec<String>, aud: &str) -> AppResult<String> {
        if !self.audiences.is_empty() && !self.audiences.iter().any(|x| x == aud) {
            return Err(AppError::internal(format!(
                "audience {aud:?} not in [auth.jwt].audiences"
            )));
        }
        self.encode_claims(&self.base_claims(sub, roles, Some(aud.to_string())))
    }

    /// 签发 token 并合并自定义 claims（`extra` 必须是 JSON 对象）。
    /// 标准/安全相关字段（sub / iss / iat / exp / orig_iat / aud / roles / nbf / jti）
    /// **不可覆盖**——若把外部可控 JSON 传入，同名覆盖可注入管理员身份或永不过期；
    /// 需要携带角色请用 [`Jwt::sign_with_roles`]。读取端：`verify_value` 拿完整
    /// claims，或 `verify` 拿 [`Claims`]（多余字段自动忽略）。
    pub fn sign_with(&self, sub: &str, extra: serde_json::Value) -> AppResult<String> {
        const RESERVED: &[&str] = &[
            "sub", "iss", "iat", "exp", "nbf", "jti", "aud", "orig_iat", "roles",
        ];
        let claims = self.base_claims(sub, Vec::new(), None);
        let mut value = serde_json::to_value(&claims)
            .map_err(|e| AppError::internal(format!("jwt claims serialize failed: {e}")))?;
        let obj = value
            .as_object_mut()
            .expect("Claims serializes to JSON object");
        match extra {
            serde_json::Value::Object(fields) => {
                for (k, v) in fields {
                    if RESERVED.contains(&k.as_str()) {
                        return Err(AppError::internal(format!(
                            "jwt extra claim `{k}` conflicts with a reserved claim"
                        )));
                    }
                    obj.insert(k, v);
                }
            }
            _ => return Err(AppError::internal("jwt extra claims must be a JSON object")),
        }
        self.encode_value(&value)
    }

    fn decode_ignore_exp(&self, token: &str) -> Result<serde_json::Value, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.validate_exp = false;
        // 与 validation() 同样校验 aud：多 audience token 走 refresh 时
        // 缺失校验配置会恒定 401（InvalidAudience）
        if !self.audiences.is_empty() {
            validation.set_audience(&self.audiences.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        }
        decode::<serde_json::Value>(token, &self.decode_key, &validation)
            .map(|data| data.claims)
    }

    /// 过期判定对齐刷新语义：leeway = 0（jsonwebtoken 默认 60s 宽限会让
    /// 「到点即过期」的刷新窗口失真）
    fn validation(&self) -> Validation {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.leeway = 0;
        if !self.audiences.is_empty() {
            validation.set_audience(&self.audiences.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        }
        validation
    }

    /// 校验 token，失败（无效/过期/签发者或 audience 不符）统一返回 401 类错误；
    /// 其中**签名合法但已过期**返回 [`AppError::TokenExpired`]（HTTP 401 +
    /// 响应体 code 6401，前端据此触发刷新）
    pub fn verify(&self, token: &str) -> AppResult<Claims> {
        let value = self.verify_value(token)?;
        serde_json::from_value(value)
            .map_err(|e| AppError::internal(format!("jwt claims decode failed: {e}")))
    }

    /// 校验 token 并返回完整 claims（含 `sign_with` 携带的自定义字段）
    pub fn verify_value(&self, token: &str) -> AppResult<serde_json::Value> {
        let value = decode::<serde_json::Value>(token, &self.decode_key, &self.validation())
            .map(|data| data.claims)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                    AppError::TokenExpired("token expired, refresh required".to_string())
                }
                _ => AppError::unauthorized(format!("invalid token: {e}")),
            })?;
        self.check_aud(&value.get("aud").and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_array().and_then(|a| a.first()).and_then(|x| x.as_str()).map(|s| s.to_string()))
        }))?;
        Ok(value)
    }

    /// 刷新 token：签名合法（允许已过期）且 `orig_iat` 仍在刷新预算窗口内时签发
    /// 新 token。新 token 的 `orig_iat` **保留原值不重置**（防无限续期）；
    /// sub / roles / 自定义 claims 原样继承。返回 `(新 token, 新 claims)`。
    pub fn refresh(&self, token: &str) -> AppResult<(String, Claims)> {
        let mut value = self
            .decode_ignore_exp(token)
            .map_err(|e| AppError::unauthorized(format!("invalid token: {e}")))?;
        let obj = match value.as_object_mut() {
            Some(o) => o,
            None => return Err(AppError::unauthorized("invalid token: claims must be an object")),
        };

        let orig_iat = obj.get("orig_iat").and_then(|v| v.as_i64()).unwrap_or(0);
        if self.max_refresh_secs <= 0 || orig_iat <= 0 {
            return Err(AppError::unauthorized("token refresh not available"));
        }
        let now = Utc::now().timestamp();
        if now >= orig_iat.saturating_add(self.max_refresh_secs) {
            return Err(AppError::unauthorized(
                "token refresh window exceeded, please login again",
            ));
        }

        // 重签：iat/exp 取当前时间，orig_iat 及其余 claims 原样继承
        obj.insert("iat".to_string(), serde_json::json!(now));
        obj.insert(
            "exp".to_string(),
            serde_json::json!(now + Duration::hours(self.expire_hours).num_seconds()),
        );
        let new_token = self.encode_value(&value)?;

        let claims: Claims = serde_json::from_value(value)
            .map_err(|e| AppError::internal(format!("jwt claims decode failed: {e}")))?;
        Ok((new_token, claims))
    }
}

/// `Authorization: Bearer <token>` → [`Identity`] 的 [`Authn`] 实现
pub struct JwtAuthn {
    jwt: Jwt,
}

impl JwtAuthn {
    pub fn new(settings: &JwtSettings) -> AppResult<Self> {
        Ok(Self { jwt: Jwt::new(settings)? })
    }

    pub fn inner(&self) -> &Jwt {
        &self.jwt
    }
}

#[async_trait::async_trait]
impl Authn for JwtAuthn {
    fn name(&self) -> &str {
        "jwt"
    }

    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>> {
        let Some(token) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        else {
            return Ok(None); // 无凭据：交给链上的下一方式
        };
        let claims = self.jwt.verify(token)?;
        Ok(Some(
            Identity::new(claims.sub.clone(), "jwt")
                .with_roles(claims.roles.clone())
                .with_claims(serde_json::to_value(&claims).unwrap_or_default()),
        ))
    }
}
