//! 身份与主体（认证的「你是谁」）。
//!
//! - [`Principal`]：已认证主体（`sub` / 角色 / 细粒度权限），可直接作为 axum 提取器。
//! - [`Identity`]：登录态载体 = `Principal` + 认证方式 + 认证时刻。
//!
//! 用户存储由业务侧实现（见 `docs/10`）；本模块只提供「身份」这一抽象。

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::app::state::AppState;
use crate::auth::jwt::Claims;
use crate::errors::AppError;

/// 认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AuthMethod {
    /// JWT（无状态）。
    Jwt,
    /// 会话 Cookie（有状态）。
    Session,
    /// 其他（由业务自定义）。
    Custom,
}

/// 已认证主体。
///
/// `perms` 支持三种写法：`"*"`（全部）、`"{对象}:{动作}"`（精确）、`"{对象}:*"`（对象级）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Principal {
    /// 主体标识（通常为用户 id 的字符串形式）。
    pub sub: String,
    /// 角色列表（用于 Casbin RBAC 的 `g(r.sub, p.sub)`）。
    pub roles: Vec<String>,
    /// 细粒度权限列表。
    pub perms: Vec<String>,
}

impl Principal {
    /// 仅带主体标识构造。
    #[must_use]
    pub fn new(sub: impl Into<String>) -> Self {
        Self {
            sub: sub.into(),
            roles: Vec::new(),
            perms: Vec::new(),
        }
    }

    /// 由整数 id 构造（`sub = id.to_string()`）。
    #[must_use]
    pub fn from_id(id: i64) -> Self {
        Self::new(id.to_string())
    }

    /// 追加角色。
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.roles.push(role.into());
        self
    }

    /// 追加多个角色。
    #[must_use]
    pub fn with_roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.roles.extend(roles.into_iter().map(Into::into));
        self
    }

    /// 追加细粒度权限。
    #[must_use]
    pub fn with_perm(mut self, perm: impl Into<String>) -> Self {
        self.perms.push(perm.into());
        self
    }

    /// 以 `sub` 解析主体 id（`sub` 非数字时为 `None`）。
    #[must_use]
    pub fn id(&self) -> Option<i64> {
        self.sub.parse().ok()
    }

    /// 是否拥有某角色。
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    /// 是否允许对 `object` 执行 `action`（内存权限检查，不查策略文件）。
    #[must_use]
    pub fn allows(&self, object: &str, action: &str) -> bool {
        let full = format!("{object}:{action}");
        let wildcard = format!("{object}:*");
        self.perms
            .iter()
            .any(|p| p == "*" || p == &full || p == &wildcard)
    }
}

/// 认证身份（登录态载体）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Identity {
    /// 已认证主体。
    pub principal: Principal,
    /// 认证方式。
    pub method: AuthMethod,
    /// 认证时刻（Unix 秒）。
    pub authenticated_at: i64,
}

impl Identity {
    /// 以 JWT 方式构造身份。
    #[must_use]
    pub fn jwt(principal: Principal, authenticated_at: i64) -> Self {
        Self {
            principal,
            method: AuthMethod::Jwt,
            authenticated_at,
        }
    }

    /// 以会话方式构造身份。
    #[must_use]
    pub fn session(principal: Principal, authenticated_at: i64) -> Self {
        Self {
            principal,
            method: AuthMethod::Session,
            authenticated_at,
        }
    }
}

/// 可选的已认证主体（未认证时为 `None`，绝不拒绝请求）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalPrincipal(pub Option<Principal>);

/// 由 `ratelimit` 层**提前验签**的 access 令牌载荷。
///
/// 限流层位于 `auth` 外侧，为了给 `per_key` 拿到可信的主体，必须先验一次签；
/// 把结果放进请求扩展后，内层的 [`authenticate`](crate::auth::authenticate)
/// 直接复用，同一请求不会验两次（`RS256` 单次验签约 60µs）。
///
/// **可信性**：请求扩展只能由框架自身的层写入，客户端无法注入；且它由
/// 验签结果构造，不改变 `authenticate` 的失败语义（验签失败根本不会写入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClaims(pub Claims);

impl OptionalPrincipal {
    /// 是否已认证。
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.0.is_some()
    }

    /// 取主体（未认证为 `None`）。
    #[must_use]
    pub fn get(&self) -> Option<&Principal> {
        self.0.as_ref()
    }
}

/// 从请求扩展中取身份。
fn identity_from_parts(parts: &mut Parts) -> Option<&Identity> {
    parts.extensions.get::<Identity>()
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        identity_from_parts(parts)
            .map(|identity| identity.principal.clone())
            .ok_or_else(|| AppError::unauthenticated("authentication required"))
    }
}

impl FromRequestParts<AppState> for Identity {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        identity_from_parts(parts)
            .cloned()
            .ok_or_else(|| AppError::unauthenticated("authentication required"))
    }
}

impl FromRequestParts<AppState> for OptionalPrincipal {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            identity_from_parts(parts).map(|identity| identity.principal.clone()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_principal_with_roles_and_perms() {
        let principal = Principal::from_id(42)
            .with_role("admin")
            .with_roles(["user"])
            .with_perm("users:read");

        assert_eq!(principal.id(), Some(42));
        assert!(principal.has_role("admin"));
        assert!(principal.has_role("user"));
        assert!(!principal.has_role("guest"));

        assert!(principal.allows("users", "read"));
        assert!(!principal.allows("users", "delete"));
    }

    #[test]
    fn should_support_wildcard_perms() {
        let object_wide = Principal::new("u").with_perm("users:*");
        assert!(object_wide.allows("users", "delete"));
        assert!(!object_wide.allows("orders", "read"));

        let all = Principal::new("u").with_perm("*");
        assert!(all.allows("anything", "at-all"));
    }

    #[test]
    fn should_return_none_id_for_non_numeric_sub() {
        assert_eq!(Principal::new("svc-a").id(), None);
    }
}
