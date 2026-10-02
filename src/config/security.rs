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

/// `[jwt]` 配置段（feature = "jwt"）。secret 为空时 JWT 服务不可用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtConfig {
    #[serde(default)]
    pub secret: String,
    #[serde(default = "default_expire_hours")]
    pub expire_hours: u64,
    #[serde(default = "default_issuer")]
    pub issuer: String,
    /// 刷新预算（小时）：自**首次签发**（orig_iat）起允许 refresh 的时长上限。
    /// token 过期后只要仍在预算窗口内即可换新 token（续期不重置 orig_iat，
    /// 防止 token 无限续期）；超窗必须重新登录。0 = 禁用刷新。
    #[serde(default = "default_max_refresh_hours")]
    pub max_refresh_hours: u64,
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            secret: String::new(),
            expire_hours: default_expire_hours(),
            issuer: default_issuer(),
            max_refresh_hours: default_max_refresh_hours(),
        }
    }
}
