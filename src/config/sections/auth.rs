//! `[auth]` 配置节：认证方式（session/jwt/oauth2）、JWT 参数、密码策略（文档 三·13）。
//!
//! 认证只回答「你是谁」；授权（能做什么）见 `[authz]`（Casbin）。

use serde::{Deserialize, Serialize};

fn default_expire_hours() -> u64 {
    24
}
fn default_issuer() -> String {
    "core-rs".to_string()
}
fn default_max_refresh_hours() -> u64 {
    168
}
fn default_cookie_name() -> String {
    "core_rs_session".to_string()
}
fn default_session_ttl() -> u64 {
    7 * 24 * 3600
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthSettings {
    /// 认证方式，逗号分隔可并存：`jwt` | `session` | `oauth2`，如 `jwt,session`。
    /// 空串 = 不启用认证中间件（Anonymous）。
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub jwt: JwtSettings,
    #[serde(default)]
    pub session: SessionSettings,
    #[serde(default)]
    pub oauth2: OAuth2Settings,
    #[serde(default)]
    pub password: PasswordPolicy,
}

impl AuthSettings {
    /// 解析 mode：返回启用的方式列表（去重、保序）
    pub fn modes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in self.mode.split(',').map(|s| s.trim().to_ascii_lowercase()) {
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

/// `[auth.jwt]`。secret 为空时 JWT 不可用；**secret 只走环境变量，不写入 toml**。
#[derive(Clone, Serialize, Deserialize)]
pub struct JwtSettings {
    #[serde(default)]
    pub secret: String,
    /// access token 有效期（小时）
    #[serde(default = "default_expire_hours")]
    pub expire_hours: u64,
    #[serde(default = "default_issuer")]
    pub issuer: String,
    /// 刷新预算（小时）：自**首次签发**（orig_iat）起允许 refresh 的时长上限，
    /// 续期不重置起点，防止 token 无限续期。0 = 禁用刷新。
    #[serde(default = "default_max_refresh_hours")]
    pub max_refresh_hours: u64,
    /// audience 列表（多端隔离：用户端 / 管理端各自 Claims）；空 = 不校验 aud
    #[serde(default)]
    pub audiences: Vec<String>,
}

impl Default for JwtSettings {
    fn default() -> Self {
        Self {
            secret: String::new(),
            expire_hours: default_expire_hours(),
            issuer: default_issuer(),
            max_refresh_hours: default_max_refresh_hours(),
            audiences: Vec::new(),
        }
    }
}

/// 手写 Debug：secret 永不输出（HS256 的 secret 是唯一防线）
impl std::fmt::Debug for JwtSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtSettings")
            .field("secret", &"***")
            .field("expire_hours", &self.expire_hours)
            .field("issuer", &self.issuer)
            .field("max_refresh_hours", &self.max_refresh_hours)
            .field("audiences", &self.audiences)
            .finish()
    }
}

/// `[auth.session]`：服务端会话，store 复用 cache 后端（单机 memory / 多实例 redis）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettings {
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,
    /// 会话 TTL（秒），滑动过期（每次访问续期）
    #[serde(default = "default_session_ttl")]
    pub ttl_secs: u64,
    /// Cookie Secure 标记（HTTPS 部署开启）
    #[serde(default)]
    pub secure: bool,
    /// Cookie Domain；空 = 仅当前 host
    #[serde(default)]
    pub domain: String,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            cookie_name: default_cookie_name(),
            ttl_secs: default_session_ttl(),
            secure: false,
            domain: String::new(),
        }
    }
}

/// `[auth.oauth2]`：OAuth2 授权码 + PKCE、第三方登录（feature = "oauth2"）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuth2Settings {
    /// 启用的 provider 名，如 `github`、`wechat`；多 provider 时在 providers 里逐个声明
    #[serde(default)]
    pub enabled_providers: Vec<String>,
    /// provider 名 → { client_id, client_secret, auth_url, token_url, userinfo_url, scopes }
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, OAuth2Provider>,
    /// 授权回调地址模板，`{provider}` 占位，如 `https://app.example.com/auth/{provider}/callback`
    #[serde(default)]
    pub redirect_url: String,
}

/// 单个 OAuth2 provider 的端点与凭证（secret 只走环境变量）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuth2Provider {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub auth_url: String,
    #[serde(default)]
    pub token_url: String,
    /// 用户信息端点（拿回的 JSON 转为 Identity claims）
    #[serde(default)]
    pub userinfo_url: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// `[auth.password]`：密码策略（注册/改密时由应用侧校验器使用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordPolicy {
    #[serde(default = "default_min_length")]
    pub min_length: usize,
    #[serde(default)]
    pub require_digit: bool,
    #[serde(default)]
    pub require_lowercase: bool,
    #[serde(default)]
    pub require_uppercase: bool,
    #[serde(default)]
    pub require_special: bool,
}

fn default_min_length() -> usize {
    8
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_length: default_min_length(),
            require_digit: false,
            require_lowercase: false,
            require_uppercase: false,
            require_special: false,
        }
    }
}

impl PasswordPolicy {
    /// 按策略检查明文密码；不满足返回第一条不合规原因
    pub fn check(&self, password: &str) -> Result<(), String> {
        if password.chars().count() < self.min_length {
            return Err(format!("密码长度不得少于 {min_length} 位", min_length = self.min_length));
        }
        let has = |f: fn(char) -> bool| password.chars().any(f);
        if self.require_digit && !has(|c| c.is_ascii_digit()) {
            return Err("密码必须包含数字".to_string());
        }
        if self.require_lowercase && !has(|c| c.is_ascii_lowercase()) {
            return Err("密码必须包含小写字母".to_string());
        }
        if self.require_uppercase && !has(|c| c.is_ascii_uppercase()) {
            return Err("密码必须包含大写字母".to_string());
        }
        if self.require_special && !has(|c| !c.is_alphanumeric()) {
            return Err("密码必须包含特殊字符".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_policy_check() {
        let p = PasswordPolicy { min_length: 8, require_digit: true, require_special: true, ..Default::default() };
        assert!(p.check("short").is_err());
        assert!(p.check("nodigitpass!").is_err());
        assert!(p.check("NoSpecial123").is_err());
        assert!(p.check("GoodPass1!").is_ok());
    }

    #[test]
    fn modes_parse() {
        let a = AuthSettings { mode: "JWT, session".into(), ..Default::default() };
        assert_eq!(a.modes(), vec!["jwt".to_string(), "session".to_string()]);
    }
}
