//! `[auth]` 配置节：认证方式（session/jwt/oauth2）、JWT 参数、密码策略（文档 三·13）。
//!
//! 认证只回答「你是谁」；授权（能做什么）见 `[authz]`（Casbin）。

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

fn default_expire_hours() -> u64 { // access token 有效期默认值函数
    24 // 默认 24 小时
}
fn default_issuer() -> String { // JWT 签发者默认值函数
    "core-rs".to_string() // 默认 core-rs
}
fn default_max_refresh_hours() -> u64 { // 刷新预算默认值函数
    168 // 默认 168 小时（7 天）
}
fn default_cookie_name() -> String { // 会话 Cookie 名默认值函数
    "core_rs_session".to_string() // 默认 cookie 名
}
fn default_session_ttl() -> u64 { // 会话 TTL 默认值函数
    7 * 24 * 3600 // 默认 7 天（秒）
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)] // 派生调试/克隆/默认值/serde
pub struct AuthSettings { // 定义 `[auth]` 配置结构体
    /// 认证方式，逗号分隔可并存：`jwt` | `session` | `oauth2`，如 `jwt,session`。
    /// 空串 = 不启用认证中间件（Anonymous）。
    #[serde(default)] // 缺省为空串（不启用认证）
    pub mode: String, // 认证方式列表
    #[serde(default)] // 缺省用 JWT 默认值
    pub jwt: JwtSettings, // JWT 参数
    #[serde(default)] // 缺省用会话默认值
    pub session: SessionSettings, // 会话参数
    #[serde(default)] // 缺省用 OAuth2 默认值
    pub oauth2: OAuth2Settings, // OAuth2 参数
    #[serde(default)] // 缺省用密码策略默认值
    pub password: PasswordPolicy, // 密码策略
}

impl AuthSettings { // 为 AuthSettings 实现方法
    /// 解析 mode：返回启用的方式列表（去重、保序）
    pub fn modes(&self) -> Vec<String> { // 解析 mode 字符串为方式列表
        let mut out: Vec<String> = Vec::new(); // 结果列表，保持顺序并去重
        for s in self.mode.split(',').map(|s| s.trim().to_ascii_lowercase()) { // 按逗号切分并去空白转小写
            if !s.is_empty() && !out.contains(&s) { // 非空且未出现过才加入
                out.push(s); // 追加到结果列表
            }
        }
        out // 返回解析结果
    }
}

/// `[auth.jwt]`。secret 为空时 JWT 不可用；**secret 只走环境变量，不写入 toml**。
#[derive(Clone, Serialize, Deserialize)] // 派生克隆与 serde（Default/Debug 手写）
pub struct JwtSettings { // 定义 `[auth.jwt]` 配置
    #[serde(default)] // 缺省为空串（JWT 不可用）
    pub secret: String, // HMAC 签名密钥
    /// access token 有效期（小时）
    #[serde(default = "default_expire_hours")] // 缺省为 24 小时
    pub expire_hours: u64, // access token 有效期（小时）
    #[serde(default = "default_issuer")] // 缺省为 core-rs
    pub issuer: String, // JWT 签发者
    /// 刷新预算（小时）：自**首次签发**（orig_iat）起允许 refresh 的时长上限，
    /// 续期不重置起点，防止 token 无限续期。0 = 禁用刷新。
    #[serde(default = "default_max_refresh_hours")] // 缺省为 168 小时
    pub max_refresh_hours: u64, // 刷新预算（小时）
    /// audience 列表（多端隔离：用户端 / 管理端各自 Claims）；空 = 不校验 aud
    #[serde(default)] // 缺省为空列表（不校验 aud）
    pub audiences: Vec<String>, // audience 列表
}

impl Default for JwtSettings { // 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            secret: String::new(), // 默认无密钥
            expire_hours: default_expire_hours(), // 默认 24 小时
            issuer: default_issuer(), // 默认 core-rs
            max_refresh_hours: default_max_refresh_hours(), // 默认 168 小时
            audiences: Vec::new(), // 默认不校验 aud
        }
    }
}

/// 手写 Debug：secret 永不输出（HS256 的 secret 是唯一防线）
impl std::fmt::Debug for JwtSettings { // 手写 Debug，secret 永不输出
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("JwtSettings") // 开始构造调试输出
            .field("secret", &"***") // secret 用 *** 占位
            .field("expire_hours", &self.expire_hours) // 输出有效期
            .field("issuer", &self.issuer) // 输出签发者
            .field("max_refresh_hours", &self.max_refresh_hours) // 输出刷新预算
            .field("audiences", &self.audiences) // 输出 audience 列表
            .finish() // 结束并生成调试输出
    }
}

/// `[auth.session]`：服务端会话，store 复用 cache 后端（单机 memory / 多实例 redis）。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct SessionSettings { // 定义 `[auth.session]` 配置
    #[serde(default = "default_cookie_name")] // 缺省为 core_rs_session
    pub cookie_name: String, // 会话 Cookie 名
    /// 会话 TTL（秒），滑动过期（每次访问续期）
    #[serde(default = "default_session_ttl")] // 缺省为 7 天
    pub ttl_secs: u64, // 会话 TTL（秒）
    /// Cookie Secure 标记（HTTPS 部署开启）
    #[serde(default)] // 缺省为 false
    pub secure: bool, // Cookie Secure 标记
    /// Cookie Domain；空 = 仅当前 host
    #[serde(default)] // 缺省为空串（仅当前 host）
    pub domain: String, // Cookie Domain
}

impl Default for SessionSettings { // 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            cookie_name: default_cookie_name(), // 默认 core_rs_session
            ttl_secs: default_session_ttl(), // 默认 7 天
            secure: false, // 默认非 Secure
            domain: String::new(), // 默认仅当前 host
        }
    }
}

/// `[auth.oauth2]`：OAuth2 授权码 + PKCE、第三方登录（feature = "oauth2"）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)] // 派生调试/克隆/serde/默认值
pub struct OAuth2Settings { // 定义 `[auth.oauth2]` 配置
    /// 启用的 provider 名，如 `github`、`wechat`；多 provider 时在 providers 里逐个声明
    #[serde(default)] // 缺省为空列表
    pub enabled_providers: Vec<String>, // 启用的 provider 名列表
    /// provider 名 → { client_id, client_secret, auth_url, token_url, userinfo_url, scopes }
    #[serde(default)] // 缺省为空映射
    pub providers: std::collections::BTreeMap<String, OAuth2Provider>, // provider 名到配置的映射
    /// 授权回调地址模板，`{provider}` 占位，如 `https://app.example.com/auth/{provider}/callback`
    #[serde(default)] // 缺省为空串
    pub redirect_url: String, // 授权回调地址模板
}

/// 单个 OAuth2 provider 的端点与凭证（secret 只走环境变量）
#[derive(Debug, Clone, Serialize, Deserialize, Default)] // 派生调试/克隆/serde/默认值
pub struct OAuth2Provider { // 定义单个 OAuth2 provider 配置
    #[serde(default)] // 缺省为空串
    pub client_id: String, // 客户端 id
    #[serde(default)] // 缺省为空串
    pub client_secret: String, // 客户端密钥
    #[serde(default)] // 缺省为空串
    pub auth_url: String, // 授权端点
    #[serde(default)] // 缺省为空串
    pub token_url: String, // 令牌端点
    /// 用户信息端点（拿回的 JSON 转为 Identity claims）
    #[serde(default)] // 缺省为空串
    pub userinfo_url: String, // 用户信息端点
    #[serde(default)] // 缺省为空列表
    pub scopes: Vec<String>, // 申请的 scope 列表
}

/// `[auth.password]`：密码策略（注册/改密时由应用侧校验器使用）。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct PasswordPolicy { // 定义 `[auth.password]` 配置
    #[serde(default = "default_min_length")] // 缺省为 8
    pub min_length: usize, // 密码最小长度
    #[serde(default)] // 缺省为 false
    pub require_digit: bool, // 是否要求数字
    #[serde(default)] // 缺省为 false
    pub require_lowercase: bool, // 是否要求小写字母
    #[serde(default)] // 缺省为 false
    pub require_uppercase: bool, // 是否要求大写字母
    #[serde(default)] // 缺省为 false
    pub require_special: bool, // 是否要求特殊字符
}

fn default_min_length() -> usize { // 最小长度默认值函数
    8 // 默认 8 位
}

impl Default for PasswordPolicy { // 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            min_length: default_min_length(), // 默认 8 位
            require_digit: false, // 默认不要求数字
            require_lowercase: false, // 默认不要求小写
            require_uppercase: false, // 默认不要求大写
            require_special: false, // 默认不要求特殊字符
        }
    }
}

impl PasswordPolicy { // 为密码策略实现方法
    /// 按策略检查明文密码；不满足返回第一条不合规原因
    pub fn check(&self, password: &str) -> Result<(), String> { // 按策略校验明文密码
        if password.chars().count() < self.min_length { // 长度不足则报错
            return Err(format!("密码长度不得少于 {min_length} 位", min_length = self.min_length)); // 返回长度不足原因
        }
        let has = |f: fn(char) -> bool| password.chars().any(f); // 定义「是否含满足条件的字符」闭包
        if self.require_digit && !has(|c| c.is_ascii_digit()) { // 要求数字但缺失
            return Err("密码必须包含数字".to_string()); // 返回缺数字原因
        }
        if self.require_lowercase && !has(|c| c.is_ascii_lowercase()) { // 要求小写但缺失
            return Err("密码必须包含小写字母".to_string()); // 返回缺小写原因
        }
        if self.require_uppercase && !has(|c| c.is_ascii_uppercase()) { // 要求大写但缺失
            return Err("密码必须包含大写字母".to_string()); // 返回缺大写原因
        }
        if self.require_special && !has(|c| !c.is_alphanumeric()) { // 要求特殊字符但缺失
            return Err("密码必须包含特殊字符".to_string()); // 返回缺特殊字符原因
        }
        Ok(()) // 全部通过，返回成功
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
