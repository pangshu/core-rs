//! JWT 签发与校验（feature = "jwt"，HS256，文档 三·13）。
//!
//! - 多 audience：用户端 / 管理端各自 Claims（`[auth.jwt].audiences` 配置后校验 aud）；
//! - 刷新机制：token 携带首次签发时间 `orig_iat`，在 `[auth.jwt].max_refresh_hours`
//!   预算内允许过期 token 换新，新 token 保留原 `orig_iat`（续期不重置起点，
//!   token 无法无限续期）；过期响应为 HTTP 401 + 响应体 `code = 6401`；
//! - [`JwtAuthn`] 把它接入统一 [`Authn`](super::Authn) 链（Bearer 头识别）。

use chrono::{Duration, Utc}; // 引入时间库：Utc 取当前时间，Duration 算过期偏移
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation}; // 引入 JWT 编解码与校验配置类型
use serde::{Deserialize, Serialize}; // 引入 serde 派生宏，用于 Claims 序列化

use axum::http::request::Parts; // 引入请求部件类型，用于读取 Authorization 头

use crate::config::sections::JwtSettings; // 引入 JWT 相关配置结构
use crate::error::{AppError, AppResult}; // 引入框架错误类型与结果别名

use super::{Authn, Identity}; // 引入认证契约与统一身份类型

/// 标准 Claims；`sign_with` 可携带自定义字段（读取端 `verify_value` 拿完整 JSON）
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 序列化
pub struct Claims { // 定义 JWT 标准载荷结构
    /// 用户标识
    pub sub: String, // subject：用户标识
    pub iss: String, // issuer：签发者
    pub iat: i64, // issued at：签发时间（秒）
    pub exp: i64, // expiration：过期时间（秒）
    /// 首次签发时间（秒）；刷新不重置。旧版本 token 无此字段时为 0（不可刷新）
    #[serde(default)] // 反序列化缺失时用默认值 0，兼容旧 token
    pub orig_iat: i64, // 首次签发时间，用于限制刷新总时长
    /// 角色列表（`sign_with_roles` / `sign_with` 携带；旧 token 无此字段时为空）
    #[serde(default)] // 缺失时默认空列表，兼容旧 token
    pub roles: Vec<String>, // 角色名列表
    /// audience（多端隔离）；未配置 audiences 时不签发/不校验
    #[serde(default, skip_serializing_if = "Option::is_none")] // 缺失默认 None，且为 None 时不输出该字段
    pub aud: Option<String>, // 受众标识，用于用户端/管理端隔离
}

#[derive(Debug, Clone)] // 派生调试与克隆
pub struct Jwt { // 定义 JWT 签发/校验器
    encode_key: EncodingKey, // HS256 签名密钥
    decode_key: DecodingKey, // HS256 验签密钥（与签名密钥同源）
    issuer: String, // 期望的签发者，校验时比对 iss
    expire_hours: i64, // token 有效期（小时）
    max_refresh_secs: i64, // 允许刷新的总时长预算（秒）
    audiences: Vec<String>, // 允许的 audience 白名单
}

impl Jwt { // 为 Jwt 提供构造与签发/校验能力
    pub fn new(settings: &JwtSettings) -> AppResult<Self> { // 由配置构造校验器，含安全校验
        if settings.secret.is_empty() { // secret 为空则无法签名
            return Err(AppError::internal( // 返回内部错误
                "auth.jwt.secret 为空：请配置后再启用 JWT 认证（secret 只走环境变量）", // 提示需配置 secret
            ));
        }
        // HS256 的 secret 是唯一防线：<32 字节可被离线爆破，签发即等于授权开放
        if settings.secret.len() < 32 { // 密钥过短则安全性不足
            return Err(AppError::internal( // 返回内部错误
                "auth.jwt.secret 过短：至少 32 字节（建议 openssl rand -base64 48）", // 提示密钥至少 32 字节
            ));
        }
        Ok(Self { // 组装校验器实例
            encode_key: EncodingKey::from_secret(settings.secret.as_bytes()), // 用密钥字节构造签名键
            decode_key: DecodingKey::from_secret(settings.secret.as_bytes()), // 用同一密钥构造验签键
            issuer: settings.issuer.clone(), // 记录期望签发者
            expire_hours: settings.expire_hours as i64, // 记录有效期（小时）
            max_refresh_secs: settings.max_refresh_hours as i64 * 3600, // 小时换算为秒作为刷新预算
            audiences: settings.audiences.clone(), // 记录 audience 白名单
        })
    }

    fn base_claims(&self, sub: &str, roles: Vec<String>, aud: Option<String>) -> Claims { // 构造基础标准 Claims
        let now = Utc::now().timestamp(); // 取当前时间戳（秒）
        Claims { // 组装 Claims
            sub: sub.to_string(), // 设置用户标识
            iss: self.issuer.clone(), // 设置签发者
            iat: now, // 设置签发时间为当前
            exp: now + Duration::hours(self.expire_hours).num_seconds(), // 过期时间 = 现在 + 有效期
            orig_iat: now, // 首次签发时间即当前（刷新时不重置）
            roles, // 写入角色列表
            aud: aud.filter(|_| !self.audiences.is_empty()), // 未配置 audiences 时丢弃 aud
        }
    }

    fn encode_value(&self, value: &serde_json::Value) -> AppResult<String> { // 把任意 JSON 载荷签成 token
        encode(&Header::new(Algorithm::HS256), value, &self.encode_key) // 用 HS256 头与密钥编码
            .map_err(|e| AppError::internal(format!("jwt sign failed: {e}"))) // 失败包装为内部错误
    }

    fn encode_claims(&self, claims: &Claims) -> AppResult<String> { // 把 Claims 序列化后签成 token
        let value = serde_json::to_value(claims) // 把 Claims 转成 JSON 值
            .map_err(|e| AppError::internal(format!("jwt claims serialize failed: {e}")))?; // 序列化失败则返回内部错误
        self.encode_value(&value) // 交由 encode_value 完成签名
    }

    /// 校验 audience 是否在配置列表（未配置 audiences 时跳过）
    fn check_aud(&self, aud: &Option<String>) -> AppResult<()> { // 校验 token 的 aud 是否被允许
        if self.audiences.is_empty() { // 未配置白名单则不校验
            return Ok(()); // 直接放行
        }
        match aud { // 按 aud 取值分派
            Some(a) if self.audiences.iter().any(|x| x == a) => Ok(()), // aud 命中白名单则放行
            other => Err(AppError::unauthorized(format!( // 否则视为未授权
                "invalid audience {other:?}" // 错误信息附带实际 aud
            ))),
        }
    }

    /// 为指定用户签发 token（无角色、无 audience）
    pub fn sign(&self, sub: &str) -> AppResult<String> { // 最简签发接口
        self.encode_claims(&self.base_claims(sub, Vec::new(), None)) // 用空角色/无 aud 的基础 Claims 签发
    }

    /// 签发 token 并携带角色列表
    pub fn sign_with_roles(&self, sub: &str, roles: Vec<String>) -> AppResult<String> { // 携带角色的签发接口
        self.encode_claims(&self.base_claims(sub, roles, None)) // 用给定角色、无 aud 的 Claims 签发
    }

    /// 签发 token 指定 audience（多端隔离：用户端 / 管理端各自 token）
    pub fn sign_for_audience(&self, sub: &str, roles: Vec<String>, aud: &str) -> AppResult<String> { // 指定受众的签发接口
        if !self.audiences.is_empty() && !self.audiences.iter().any(|x| x == aud) { // 配置了白名单但 aud 不在其中
            return Err(AppError::internal(format!( // 返回内部错误
                "audience {aud:?} not in [auth.jwt].audiences" // 提示 aud 不在白名单
            )));
        }
        self.encode_claims(&self.base_claims(sub, roles, Some(aud.to_string()))) // 用指定 aud 的 Claims 签发
    }

    /// 签发 token 并合并自定义 claims（`extra` 必须是 JSON 对象）。
    /// 标准/安全相关字段（sub / iss / iat / exp / orig_iat / aud / roles / nbf / jti）
    /// **不可覆盖**——若把外部可控 JSON 传入，同名覆盖可注入管理员身份或永不过期；
    /// 需要携带角色请用 [`Jwt::sign_with_roles`]。读取端：`verify_value` 拿完整
    /// claims，或 `verify` 拿 [`Claims`]（多余字段自动忽略）。
    pub fn sign_with(&self, sub: &str, extra: serde_json::Value) -> AppResult<String> { // 合并自定义字段的签发接口
        const RESERVED: &[&str] = &[ // 定义不可被覆盖的保留字段黑名单
            "sub", "iss", "iat", "exp", "nbf", "jti", "aud", "orig_iat", "roles", // 保留字段清单
        ];
        let claims = self.base_claims(sub, Vec::new(), None); // 先生成基础标准 Claims
        let mut value = serde_json::to_value(&claims) // 转成可改写的 JSON 值
            .map_err(|e| AppError::internal(format!("jwt claims serialize failed: {e}")))?; // 序列化失败返回内部错误
        let obj = value // 取 JSON 对象以便插入自定义字段
            .as_object_mut() // 获取可变对象引用
            .expect("Claims serializes to JSON object"); // Claims 必然序列化为对象，否则为程序错误
        match extra { // 按 extra 类型分派
            serde_json::Value::Object(fields) => { // extra 必须是对象
                for (k, v) in fields { // 遍历自定义字段
                    if RESERVED.contains(&k.as_str()) { // 命中保留字段则拒绝
                        return Err(AppError::internal(format!( // 返回内部错误
                            "jwt extra claim `{k}` conflicts with a reserved claim" // 提示字段与保留字段冲突
                        )));
                    }
                    obj.insert(k, v); // 安全字段插入对象
                }
            }
            _ => return Err(AppError::internal("jwt extra claims must be a JSON object")), // 非对象则报错
        }
        self.encode_value(&value) // 用合并后的载荷签名
    }

    fn decode_ignore_exp(&self, token: &str) -> Result<serde_json::Value, jsonwebtoken::errors::Error> { // 验签但忽略过期，用于刷新流程
        let mut validation = Validation::new(Algorithm::HS256); // 新建 HS256 校验配置
        validation.set_issuer(&[self.issuer.as_str()]); // 校验签发者
        validation.validate_exp = false; // 关闭过期校验，允许过期 token 换新
        // 与 validation() 同样校验 aud：多 audience token 走 refresh 时
        // 缺失校验配置会恒定 401（InvalidAudience）
        if !self.audiences.is_empty() { // 配置了 audience 白名单时
            validation.set_audience(&self.audiences.iter().map(|s| s.as_str()).collect::<Vec<_>>()); // 设置允许的 aud 列表
        }
        decode::<serde_json::Value>(token, &self.decode_key, &validation) // 解码为 JSON 值
            .map(|data| data.claims) // 仅取 claims 部分
    }

    /// 过期判定对齐刷新语义：leeway = 0（jsonwebtoken 默认 60s 宽限会让
    /// 「到点即过期」的刷新窗口失真）
    fn validation(&self) -> Validation { // 构造常规校验配置（含过期校验）
        let mut validation = Validation::new(Algorithm::HS256); // 新建 HS256 校验配置
        validation.set_issuer(&[self.issuer.as_str()]); // 校验签发者
        validation.leeway = 0; // 关闭宽限期，做到到点即过期
        if !self.audiences.is_empty() { // 配置了 audience 白名单时
            validation.set_audience(&self.audiences.iter().map(|s| s.as_str()).collect::<Vec<_>>()); // 设置允许的 aud 列表
        }
        validation // 返回配置
    }

    /// 校验 token，失败（无效/过期/签发者或 audience 不符）统一返回 401 类错误；
    /// 其中**签名合法但已过期**返回 [`AppError::TokenExpired`]（HTTP 401 +
    /// 响应体 code 6401，前端据此触发刷新）
    pub fn verify(&self, token: &str) -> AppResult<Claims> { // 校验 token 并解析为 Claims
        let value = self.verify_value(token)?; // 先做完整校验得到 JSON
        serde_json::from_value(value) // 再反序列化为 Claims（多余字段忽略）
            .map_err(|e| AppError::internal(format!("jwt claims decode failed: {e}"))) // 解析失败返回内部错误
    }

    /// 校验 token 并返回完整 claims（含 `sign_with` 携带的自定义字段）
    pub fn verify_value(&self, token: &str) -> AppResult<serde_json::Value> { // 校验并返回原始 JSON claims
        let value = decode::<serde_json::Value>(token, &self.decode_key, &self.validation()) // 用常规配置解码
            .map(|data| data.claims) // 取 claims
            .map_err(|e| match e.kind() { // 按错误类型映射
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => { // 签名合法但过期
                    AppError::TokenExpired("token expired, refresh required".to_string()) // 映射为 TokenExpired（6401）
                }
                _ => AppError::unauthorized(format!("invalid token: {e}")), // 其余映射为 401 未授权
            })?;
        self.check_aud(&value.get("aud").and_then(|v| { // 从 claims 提取 aud 并校验
            v.as_str() // 若 aud 为字符串直接取
                .map(|s| s.to_string()) // 转成拥有所有权的字符串
                .or_else(|| v.as_array().and_then(|a| a.first()).and_then(|x| x.as_str()).map(|s| s.to_string())) // 若为数组则取第一个元素
        }))?;
        Ok(value) // 返回完整 claims
    }

    /// 刷新 token：签名合法（允许已过期）且 `orig_iat` 仍在刷新预算窗口内时签发
    /// 新 token。新 token 的 `orig_iat` **保留原值不重置**（防无限续期）；
    /// sub / roles / 自定义 claims 原样继承。返回 `(新 token, 新 claims)`。
    pub fn refresh(&self, token: &str) -> AppResult<(String, Claims)> { // 定义刷新流程
        let mut value = self // 解码旧 token（忽略过期）
            .decode_ignore_exp(token) // 验签但不管过期
            .map_err(|e| AppError::unauthorized(format!("invalid token: {e}")))?; // 验签失败返回 401
        let obj = match value.as_object_mut() { // 取可变 JSON 对象
            Some(o) => o, // 是对象则使用
            None => return Err(AppError::unauthorized("invalid token: claims must be an object")), // 非对象则拒绝
        };

        let orig_iat = obj.get("orig_iat").and_then(|v| v.as_i64()).unwrap_or(0); // 读取首次签发时间，缺失记为 0
        if self.max_refresh_secs <= 0 || orig_iat <= 0 { // 未配置刷新预算或旧 token 无 orig_iat
            return Err(AppError::unauthorized("token refresh not available")); // 不允许刷新
        }
        let now = Utc::now().timestamp(); // 取当前时间戳
        if now >= orig_iat.saturating_add(self.max_refresh_secs) { // 超出刷新总预算窗口
            return Err(AppError::unauthorized( // 返回 401 未授权
                "token refresh window exceeded, please login again", // 提示刷新窗口已过需重新登录
            ));
        }

        // 重签：iat/exp 取当前时间，orig_iat 及其余 claims 原样继承
        obj.insert("iat".to_string(), serde_json::json!(now)); // 更新签发时间为当前
        obj.insert( // 更新过期时间
            "exp".to_string(), // 字段名 exp
            serde_json::json!(now + Duration::hours(self.expire_hours).num_seconds()), // 现在 + 有效期
        );
        let new_token = self.encode_value(&value)?; // 用继承的 claims 重签新 token

        let claims: Claims = serde_json::from_value(value) // 把最终 claims 反序列化
            .map_err(|e| AppError::internal(format!("jwt claims decode failed: {e}")))?; // 失败返回内部错误
        Ok((new_token, claims)) // 返回新 token 与其 claims
    }
}

/// `Authorization: Bearer <token>` → [`Identity`] 的 [`Authn`] 实现
pub struct JwtAuthn { // 把 Jwt 接入统一认证链的适配器
    jwt: Jwt, // 内部持有的 JWT 校验器
}

impl JwtAuthn { // 为 JwtAuthn 提供构造与访问
    pub fn new(settings: &JwtSettings) -> AppResult<Self> { // 由配置构造
        Ok(Self { jwt: Jwt::new(settings)? }) // 复用 Jwt::new 完成构造
    }

    pub fn inner(&self) -> &Jwt { // 暴露内部 Jwt，供签发/刷新等使用
        &self.jwt // 返回内部引用
    }
}

#[async_trait::async_trait] // 为下面的实现启用 async 支持
impl Authn for JwtAuthn { // 让 JwtAuthn 实现认证契约
    fn name(&self) -> &str { // 返回认证方式名
        "jwt" // 固定标识为 jwt
    }

    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>> { // 从 Bearer 头识别身份
        let Some(token) = parts // 尝试从请求头提取 token
            .headers // 访问请求头集合
            .get(axum::http::header::AUTHORIZATION) // 取 Authorization 头
            .and_then(|v| v.to_str().ok()) // 转成合法字符串
            .and_then(|v| v.strip_prefix("Bearer ")) // 去掉 Bearer 前缀得到 token
        else { // let-else：无 token 时走 else 分支
            return Ok(None); // 无凭据：交给链上的下一方式
        };
        let claims = self.jwt.verify(token)?; // 校验 token 并解析 Claims（过期/无效会返回 401）
        Ok(Some( // 构造统一身份
            Identity::new(claims.sub.clone(), "jwt") // 以 sub 为用户 id，来源标记 jwt
                .with_roles(claims.roles.clone()) // 带上角色列表
                .with_claims(serde_json::to_value(&claims).unwrap_or_default()), // 带上完整 claims JSON
        ))
    }
}
