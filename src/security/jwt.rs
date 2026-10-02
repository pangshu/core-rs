//! JWT 签发与校验（HS256）。`[jwt].secret` 为空时 [`Jwt::new`] 返回内部错误。
//!
//! 刷新机制（对标 go-admin-core jwtauth）：token 携带首次签发时间 `orig_iat`，
//! [`Jwt::refresh`] 在 `[jwt].max_refresh_hours` 预算内允许过期 token 换新，
//! 新 token 保留原 `orig_iat`（续期不重置起点，token 无法无限续期）；
//! 过期响应为 HTTP 401 + 响应体 `code = 6401`，前端据此区分「该刷新了」与「未认证」。

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::config::JwtConfig;
use crate::error::{AppError, AppResult};

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
}

#[derive(Debug, Clone)]
pub struct Jwt {
    encode_key: EncodingKey,
    decode_key: DecodingKey,
    issuer: String,
    expire_hours: i64,
    max_refresh_secs: i64,
}

impl Jwt {
    pub fn new(cfg: &JwtConfig) -> AppResult<Self> {
        if cfg.secret.is_empty() {
            return Err(AppError::internal(
                "jwt.secret 为空：请在 app.yml 配置 [jwt].secret 后再启用认证",
            ));
        }
        Ok(Self {
            encode_key: EncodingKey::from_secret(cfg.secret.as_bytes()),
            decode_key: DecodingKey::from_secret(cfg.secret.as_bytes()),
            issuer: cfg.issuer.clone(),
            expire_hours: cfg.expire_hours as i64,
            max_refresh_secs: cfg.max_refresh_hours as i64 * 3600,
        })
    }

    fn base_claims(&self, sub: &str, roles: Vec<String>) -> Claims {
        let now = Utc::now().timestamp();
        Claims {
            sub: sub.to_string(),
            iss: self.issuer.clone(),
            iat: now,
            exp: now + Duration::hours(self.expire_hours).num_seconds(),
            orig_iat: now,
            roles,
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

    /// 为指定用户签发 token（无角色）
    pub fn sign(&self, sub: &str) -> AppResult<String> {
        self.encode_claims(&self.base_claims(sub, Vec::new()))
    }

    /// 签发 token 并携带角色列表
    pub fn sign_with_roles(&self, sub: &str, roles: Vec<String>) -> AppResult<String> {
        self.encode_claims(&self.base_claims(sub, roles))
    }

    /// 签发 token 并合并自定义 claims（`extra` 必须是 JSON 对象；与标准字段同名时覆盖，
    /// 可用于携带 roles 之外的业务字段）。读取端：[`Jwt::verify_value`] 拿完整 claims，
    /// 或 `verify` 拿 [`Claims`]（多余字段自动忽略）。
    pub fn sign_with(&self, sub: &str, extra: serde_json::Value) -> AppResult<String> {
        let claims = self.base_claims(sub, Vec::new());
        let mut value = serde_json::to_value(&claims)
            .map_err(|e| AppError::internal(format!("jwt claims serialize failed: {e}")))?;
        let obj = value
            .as_object_mut()
            .expect("Claims serializes to JSON object");
        match extra {
            serde_json::Value::Object(fields) => {
                for (k, v) in fields {
                    obj.insert(k, v);
                }
            }
            _ => {
                return Err(AppError::internal(
                    "jwt extra claims must be a JSON object",
                ))
            }
        }
        self.encode_value(&value)
    }

    fn decode_ignore_exp(&self, token: &str) -> Result<serde_json::Value, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.validate_exp = false;
        decode::<serde_json::Value>(token, &self.decode_key, &validation)
            .map(|data| data.claims)
    }

    /// 过期判定与 Go jwt-go 对齐：leeway = 0（jsonwebtoken 默认 60s 宽限，
    /// 会让「到点即过期」的刷新语义失真）
    fn validation(&self) -> Validation {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.leeway = 0;
        validation
    }

    /// 校验 token，失败（无效/过期/签发者不符）统一返回 401 类错误；
    /// 其中**签名合法但已过期**返回 [`AppError::TokenExpired`]（HTTP 401 +
    /// 响应体 code 6401，前端据此触发刷新）
    pub fn verify(&self, token: &str) -> AppResult<Claims> {
        self.verify_value(token).and_then(|value| {
            serde_json::from_value(value)
                .map_err(|e| AppError::internal(format!("jwt claims decode failed: {e}")))
        })
    }

    /// 校验 token 并返回完整 claims（含 `sign_with` 携带的自定义字段）。
    /// 只需要 sub/roles 等标准字段时用 [`Jwt::verify`] 即可。
    pub fn verify_value(&self, token: &str) -> AppResult<serde_json::Value> {
        decode::<serde_json::Value>(token, &self.decode_key, &self.validation())
            .map(|data| data.claims)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                    AppError::TokenExpired("token expired, refresh required".to_string())
                }
                _ => AppError::unauthorized(format!("invalid token: {e}")),
            })
    }

    /// 刷新 token：签名合法（允许已过期）且 `orig_iat` 仍在刷新预算
    /// （`[jwt].max_refresh_hours`）窗口内时签发新 token。
    /// - 新 token 的 `orig_iat` **保留原值不重置**（防无限续期，超窗必须重新登录）；
    /// - sub / roles / 自定义 claims 原样继承（刷新不要求重新认证回调）；
    /// - 超窗或 token 无效统一返回 401 类错误。
    ///
    /// 返回 `(新 token, 新 claims)`。
    pub fn refresh(&self, token: &str) -> AppResult<(String, Claims)> {
        let mut value = self
            .decode_ignore_exp(token)
            .map_err(|e| AppError::unauthorized(format!("invalid token: {e}")))?;
        let obj = match value.as_object_mut() {
            Some(o) => o,
            None => return Err(AppError::unauthorized("invalid token: claims must be an object")),
        };

        let orig_iat = obj
            .get("orig_iat")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
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
