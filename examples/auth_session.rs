//! auth_session —— 会话认证 + CSRF 双提交 Cookie。
//!
//! 运行：`cargo run --example auth_session --features "session,csrf"`
//!
//! ```bash
//! # 1) 先拿 CSRF token（GET 会下发 csrf_token Cookie，前端可读）
//! curl -s -c /tmp/jar http://127.0.0.1:8094/api/v1/whoami
//! CSRF=$(awk '$6=="csrf_token" {print $7}' /tmp/jar)
//!
//! # 2) 登录：带 Cookie + X-CSRF-Token（缺任一个 → 403 / 20002）
//! curl -s -b /tmp/jar -c /tmp/jar -X POST http://127.0.0.1:8094/api/v1/login \
//!      -H "x-csrf-token: $CSRF" -H 'content-type: application/json' \
//!      -d '{"username":"admin","password":"admin123"}'
//!
//! # 3) 会话生效（Cookie: sid）
//! curl -s -b /tmp/jar http://127.0.0.1:8094/api/v1/me
//!
//! # 4) 登出（清除会话 Cookie）
//! curl -s -b /tmp/jar -c /tmp/jar -X POST http://127.0.0.1:8094/api/v1/logout \
//!      -H "x-csrf-token: $CSRF"
//! ```
//!
//! 要点：
//! - 登录成功后**重新生成 session id**（防会话固定攻击），旧 id 作废
//! - 会话 Cookie 为 `HttpOnly`，前端读不到；CSRF token 反过来**必须可读**（双提交）
//! - 本示例 `secure = false` 以支持本地 HTTP；生产必须开启（仅 HTTPS 传输）

use std::sync::Arc;

use axum::middleware::from_fn;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use aurora::auth::{Session, session_middleware};
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

/// `POST /api/v1/login` —— 校验账号并建立会话（需通过 CSRF 校验）。
async fn login(
    session: Session,
    Json(request): Json<LoginRequest>,
) -> AppResult<ApiResponse<serde_json::Value>> {
    if request.username != "admin" || request.password != "admin123" {
        return Err(AppError::biz(
            ErrorCode::UNAUTHENTICATED,
            "用户名或密码错误",
        ));
    }

    let principal = Principal::new("1");
    // 重新生成 session id：防会话固定攻击
    session.login(&principal).await?;

    Ok(ApiResponse::ok(json!({
        "logged_in": true,
        "sub": principal.sub,
    })))
}

/// `POST /api/v1/logout` —— 删除会话并清除 Cookie。
async fn logout(session: Session) -> AppResult<ApiResponse<&'static str>> {
    session.logout().await?;
    Ok(ApiResponse::ok("logged out"))
}

/// `GET /api/v1/me` —— 受保护：未登录返回 `20001`（401）。
async fn me(session: Session) -> AppResult<ApiResponse<serde_json::Value>> {
    let principal = session.require_principal().await?;
    Ok(ApiResponse::ok(json!({
        "sub": principal.sub,
        "auth": "session",
        "session_id_len": session.id().await.len(),
    })))
}

/// `GET /api/v1/whoami` —— 可选登录：未登录返回 `null`。
async fn whoami(session: Session) -> AppResult<ApiResponse<serde_json::Value>> {
    Ok(ApiResponse::ok(match session.principal().await? {
        Some(principal) => json!({ "sub": principal.sub }),
        None => json!(null),
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = preset::load_env_var("AURORA_ENV", "config").unwrap_or_default();
    config.server.port = 8094;

    // 会话认证
    config.auth.session.enabled = true;
    config.auth.session.secure = false; // 本地 HTTP 演示；生产必须 true

    // CSRF 双提交 Cookie（写方法要求 Cookie 与 X-CSRF-Token 一致）
    config.http.csrf.enabled = true;
    config.validate()?;

    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    // 会话中间件：读 Cookie → 加载主体 → 注入 `Session` 扩展
    let session_config = config.auth.session.clone();
    let store = Arc::clone(state.auth.sessions());

    // 会话中间件对两组都要生效，故先各自套层再分组：
    // - 登录公开（还没有会话）；
    // - 其余受保护（守卫层要求 `Identity`，本示例由 `whoami` 自行读会话，
    //   真实项目应在登录后签发 JWT / 注入 Identity）。
    let public = Router::new()
        .route("/api/v1/login", post(login))
        .layer(from_fn({
            let config = session_config.clone();
            let store = Arc::clone(&store);
            move |req, next| session_middleware(config.clone(), Arc::clone(&store), req, next)
        }));
    let protected = Router::new()
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/me", get(me))
        .route("/api/v1/whoami", get(whoami))
        .layer(from_fn(move |req, next| {
            session_middleware(session_config.clone(), Arc::clone(&store), req, next)
        }));
    let routes = AppRoutes::all_public(public).protected(|_| protected);

    let app = build_router(routes, state);

    println!(
        "会话 Cookie：{}（HttpOnly，secure={}）",
        config.auth.session.cookie_name, config.auth.session.secure
    );
    println!(
        "CSRF Cookie：{}（可读，双提交）",
        config.http.csrf.cookie_name
    );
    println!("会话 TTL：{:?}", config.auth.session.ttl);

    let addr = config.server.socket_addr()?;
    println!("\n监听 {addr}（先 GET /api/v1/whoami 取 CSRF token）");
    serve(app, addr).await?;
    Ok(())
}
