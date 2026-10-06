//! 认证（你是谁，文档 三·13）：session / jwt / oauth2，由配置选择，可并存。
//!
//! 认证只回答「你是谁」——统一产出 [`Identity`]（用户 id、角色、claims），
//! 由 `middleware/auth` 注入 extension，业务侧只认 `CurrentUser` 提取器，
//! 不关心底层是哪种认证方式。授权（能做什么）见 `authz/`（Casbin RBAC）。
//!
//! - [`jwt`]（feature = "jwt"）：无状态令牌，`Authorization: Bearer`；
//! - [`session`]（feature = "session"）：服务端会话，store 复用 cache 后端，
//!   凭 Cookie 的 session id 识别，适合需要**即时吊销**的场景；
//! - [`oauth2`]（feature = "oauth2"）：授权码 + PKCE 第三方登录客户端；
//!   换取用户信息后由应用落成 session / jwt，仍是统一 `Identity`。
//!
//! 凭据无效 → `Err`（401）；无凭据 → `Ok(None)`（交给下一方式）。
//! 链式组合（[`ChainAuthn`]）中单个方式的凭据错误**不阻断**其余方式：
//! 如 Bearer 已过期但 session cookie 有效时仍能以 session 登录，
//! 全部失败才返回第一个错误（保留 TokenExpired → 6401 语义）。

pub mod password;

#[cfg(feature = "session")]
pub mod session;
#[cfg(feature = "jwt")]
pub mod jwt;
#[cfg(feature = "oauth2")]
pub mod oauth2;

use axum::http::request::Parts;

use crate::error::AppResult;

/// 认证身份（框架统一产物；session 存储需要 serde 序列化）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    /// 用户标识（业务侧的 user id 字符串化）
    pub id: String,
    /// 角色列表（authz 层与业务权限判断共用）
    pub roles: Vec<String>,
    /// 完整 claims（JWT 自定义字段 / OAuth2 userinfo / session 快照）
    pub claims: serde_json::Value,
    /// 认证来源："jwt" | "session" | "oauth2:<provider>"
    pub source: String,
}

impl Identity {
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            roles: Vec::new(),
            claims: serde_json::Value::Null,
            source: source.into(),
        }
    }

    pub fn with_roles(mut self, roles: Vec<String>) -> Self {
        self.roles = roles;
        self
    }

    pub fn with_claims(mut self, claims: serde_json::Value) -> Self {
        self.claims = claims;
        self
    }
}

/// 认证方式契约
#[async_trait::async_trait]
pub trait Authn: Send + Sync {
    fn name(&self) -> &str;
    /// 从请求中识别身份。无凭据 → `None`；凭据无效 → `Err`（401）。
    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>>;
}

/// 组合认证：按配置顺序（`[auth].mode`）逐个尝试，任一命中即返回
pub struct ChainAuthn {
    schemes: Vec<std::sync::Arc<dyn Authn>>,
}

impl ChainAuthn {
    pub fn new(schemes: Vec<std::sync::Arc<dyn Authn>>) -> Self {
        Self { schemes }
    }
}

#[async_trait::async_trait]
impl Authn for ChainAuthn {
    fn name(&self) -> &str {
        "chain"
    }

    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>> {
        // 单个 scheme 凭据无效不阻断其余 scheme：Bearer 过期 + session cookie
        // 有效时应能用 session 登录，而不是被硬 401 登出
        let mut first_err: Option<crate::error::AppError> = None;
        for scheme in &self.schemes {
            match scheme.authenticate(parts).await {
                Ok(Some(identity)) => return Ok(Some(identity)),
                Ok(None) => {}
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }
}

/// 匿名认证（不启用任何方式时的占位）：恒返回 None，接口全匿名，
/// 需要登录态的接口由 `CurrentUser` 提取器返回 401
pub struct Anonymous;

#[async_trait::async_trait]
impl Authn for Anonymous {
    fn name(&self) -> &str {
        "anonymous"
    }
    async fn authenticate(&self, _parts: &Parts) -> AppResult<Option<Identity>> {
        Ok(None)
    }
}

/// 按 `[auth]` 配置装配认证链（App 装配时自动调用；cache 供 session store 使用）
#[allow(unused_variables)]
pub fn build(
    settings: &crate::config::sections::AuthSettings,
    cache: &crate::cache::CacheHandle,
) -> AppResult<Option<std::sync::Arc<dyn Authn>>> {
    #[allow(unused_mut)] // 部分 feature 关闭时无 push
    let mut schemes: Vec<std::sync::Arc<dyn Authn>> = Vec::new();
    for mode in settings.modes() {
        match mode.as_str() {
            "jwt" => {
                #[cfg(feature = "jwt")]
                if !settings.jwt.secret.is_empty() {
                    schemes.push(std::sync::Arc::new(jwt::JwtAuthn::new(&settings.jwt)?));
                    continue;
                }
                let _ = &settings;
                tracing::warn!("auth.mode includes jwt but [auth.jwt].secret is empty / feature disabled");
            }
            "session" => {
                #[cfg(feature = "session")]
                {
                    schemes.push(std::sync::Arc::new(session::SessionAuthn::new(
                        std::sync::Arc::new(session::SessionManager::new(
                            cache.clone(),
                            &settings.session,
                        )),
                        &settings.session,
                    )));
                    continue;
                }
                #[cfg(not(feature = "session"))]
                tracing::warn!("auth.mode includes session but feature `session` is disabled");
            }
            "oauth2" => {
                // OAuth2 是「换取身份」的客户端流程，不作为请求认证方式参与链；
                // 应用在回调里用其结果落 session / jwt（见 oauth2 模块文档）
            }
            other => {
                tracing::warn!(mode = %other, "unknown auth.mode, ignored");
            }
        }
    }
    if schemes.is_empty() {
        Ok(None)
    } else if schemes.len() == 1 {
        Ok(schemes.into_iter().next())
    } else {
        Ok(Some(std::sync::Arc::new(ChainAuthn::new(schemes))))
    }
}
