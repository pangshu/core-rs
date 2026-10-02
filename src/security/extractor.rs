//! `CurrentUser` 提取器：读取 `Authorization: Bearer <token>`，校验失败/缺失自动 401。

use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::error::{AppError, AppResult};
use crate::security::jwt::{Claims, Jwt};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: String,
    pub claims: Claims,
}

impl CurrentUser {
    /// token 中是否携带指定角色
    pub fn has_role(&self, role: &str) -> bool {
        self.claims.roles.iter().any(|r| r == role)
    }

    /// 校验当前用户具备任一角色，不满足返回 403。handler 里的权限检查姿势：
    ///
    /// ```no_run
    /// # use core_rs::prelude::*;
    /// # async fn admin_op(user: CurrentUser) -> ApiResult<&'static str> {
    /// user.require_any_role(&["admin", "ops"])?;
    /// Ok(ApiResponse::ok("ok"))
    /// # }
    /// ```
    pub fn require_any_role(&self, roles: &[&str]) -> AppResult<()> {
        if roles.is_empty() || self.claims.roles.iter().any(|r| roles.contains(&r.as_str())) {
            return Ok(());
        }
        Err(AppError::forbidden(format!(
            "requires one of roles {roles:?}, token roles are {:?}",
            self.claims.roles
        )))
    }
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let st = AppState::from_ref(state);
        let jwt: &Jwt = st
            .jwt
            .as_ref()
            .ok_or_else(|| AppError::internal("jwt not configured"))?;

        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::unauthorized("missing bearer token"))?;

        let claims = jwt.verify(token)?;
        Ok(Self {
            id: claims.sub.clone(),
            claims,
        })
    }
}
