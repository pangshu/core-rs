//! 认证 feature：登录签发 JWT + 受保护接口。
//!
//! ⚠️ 示例账号硬编码为 `admin` / `admin123`，**仅用于演示**；真实项目应查库校验
//! 密码哈希（`security` feature 的 `verify_password`）并接入 `[auth]` 配置。

use axum::extract::State;
use serde::{Deserialize, Serialize};

use aurora::http::Json;
use aurora::prelude::*;

/// 登录请求。
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// 用户名。
    pub username: String,
    /// 密码（明文仅用于演示）。
    pub password: String,
}

/// 登录响应（与 `TokenPair` 对齐）。
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// 访问令牌。
    pub access_token: String,
    /// 刷新令牌。
    pub refresh_token: String,
    /// 令牌类型。
    pub token_type: &'static str,
    /// 有效期（秒）。
    pub expires_in: i64,
}

/// `POST /api/v1/auth/login` —— 校验示例账号并签发令牌。
pub async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> AppResult<ApiResponse<LoginResponse>> {
    if request.username != "admin" || request.password != "admin123" {
        // 统一提示，避免用户枚举（docs/11 §6）
        return Err(AppError::biz(
            ErrorCode::UNAUTHENTICATED,
            "用户名或密码错误",
        ));
    }

    let tokens = state.auth.sign_jwt(&Principal::new("1"))?;
    Ok(ApiResponse::ok(LoginResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        token_type: tokens.token_type,
        expires_in: tokens.expires_in,
    }))
}

/// `GET /api/v1/me` —— 受保护接口：`Identity` 提取器要求已认证（否则 `20001`）。
pub async fn me(identity: Identity) -> AppResult<ApiResponse<serde_json::Value>> {
    Ok(ApiResponse::ok(serde_json::json!({
        "sub": identity.principal.sub,
        "roles": identity.principal.roles,
        "perms": identity.principal.perms,
        "method": format!("{:?}", identity.method),
        "authenticated_at": identity.authenticated_at,
    })))
}
