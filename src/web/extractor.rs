//! 增强提取器：PageQuery（分页参数）、ClientIp（客户端 IP）、CurrentUser（当前用户）。

use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use serde::Deserialize;

use crate::error::{AppError, AppResult};

fn default_page() -> u64 {
    1
}
fn default_size() -> u64 {
    10
}

/// 分页请求参数。handler 签名直接写 `q: PageQuery` 即可。
#[derive(Debug, Clone, Deserialize)]
pub struct PageQuery {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_size")]
    pub size: u64,
}

impl PageQuery {
    /// ORM 分页用的 0 起始页码。page 最小按 1 计（0 视为第一页）。
    pub fn page_index(&self) -> u64 {
        self.page.max(1) - 1
    }

    /// 每页条数，限幅 1..=100（防止超大 size 拖垮数据库）。
    /// 所有分页路径都应经此取值，防止 `LIMIT 0`。
    pub fn limit(&self) -> u64 {
        self.size.clamp(1, 100)
    }
}

impl<S> FromRequestParts<S> for PageQuery
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<PageQuery>::from_request_parts(parts, state)
            .await
            .map(|Query(q)| q)
            .map_err(|rej| AppError::bad_request(rej.body_text()))
    }
}

/// 客户端 IP：反代头（X-Forwarded-For / X-Real-IP / Forwarded）优先，回退对端地址。
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub std::net::IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        crate::utils::client_ip::resolve(&parts.headers, peer)
            .map(ClientIp)
            .ok_or_else(|| AppError::bad_request("cannot resolve client ip"))
    }
}

/// 当前用户：auth 中间件注入的 [`Identity`](crate::auth::Identity)。
/// 缺失 / 未认证统一 401，handler 不用再判空。
///
/// ```no_run
/// # use core_rs::prelude::*;
/// # async fn me(user: CurrentUser) -> ApiResult<String> {
/// Ok(ApiResponse::ok(user.id))
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: String,
    pub roles: Vec<String>,
    /// 完整 claims（JWT 自定义字段 / OAuth2 userinfo / session 快照）
    pub claims: serde_json::Value,
}

impl CurrentUser {
    pub fn from_identity(identity: &crate::auth::Identity) -> Self {
        Self {
            id: identity.id.clone(),
            roles: identity.roles.clone(),
            claims: identity.claims.clone(),
        }
    }

    /// token / 会话中是否携带指定角色
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    /// 校验当前用户具备任一角色，不满足返回 403
    pub fn require_any_role(&self, roles: &[&str]) -> AppResult<()> {
        if roles.is_empty() || self.roles.iter().any(|r| roles.contains(&r.as_str())) {
            return Ok(());
        }
        Err(AppError::forbidden(format!(
            "requires one of roles {roles:?}, got {:?}",
            self.roles
        )))
    }
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<crate::auth::Identity>()
            .map(CurrentUser::from_identity)
            .ok_or_else(|| AppError::unauthorized("authentication required"))
    }
}
