//! auth_guard —— M3 示例：登录签发 JWT + 受保护路由 + RBAC 权限校验。
//!
//! 运行：`cargo run --example auth_guard --features security`
//!
//! 演示：
//! - `security::verify_password`（Argon2id）校验口令，失败**统一提示**以防用户枚举
//! - `AuthService::sign_jwt` 签发 access + refresh 双令牌
//! - `Principal` 提取器：无 / 错 token → `401`（`20001`）
//! - `require_permission!` / `AuthService::require`：无权限 → `403`（`20004`）
//!
//! 用户表为演示用的内存数据（生产应查 `db`）；`[auth].jwt.secret` 经环境变量
//! `AURORA_AUTH__JWT__SECRET` 注入。
//!
//! ```bash
//! TOKEN=$(curl -s -X POST -H 'Content-Type: application/json' \
//!     -d '{"email":"alice@example.com","password":"correct horse battery"}' \
//!     http://127.0.0.1:8080/login | jq -r .data.access_token)
//! curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/me
//! curl -s -X DELETE -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/users/1
//! ```

use axum::extract::{Path, State};
use serde::Deserialize;
use std::sync::OnceLock;

use aurora::auth::{Principal, TokenPair};
use aurora::http::{ApiResponse, Json};
use aurora::prelude::*;
use aurora::security::{Argon2Params, hash_password, verify_password_or_dummy};

#[derive(Debug)]
struct DemoUser {
    id: i64,
    email: &'static str,
    password_hash: String,
    role: &'static str,
}

/// 演示用户表（生产应查数据库）。
fn demo_users() -> &'static [DemoUser] {
    static USERS: OnceLock<Vec<DemoUser>> = OnceLock::new();
    USERS.get_or_init(|| {
        // 用最小参数加速示例启动；生产请用 `[security].password.*` 的推荐值
        // （参数有下限，`new` 会拒绝 `time_cost < 2` 这类弱配置）
        let params = Argon2Params::new(8 * 1024, 2, 1).unwrap_or_default();
        vec![
            DemoUser {
                id: 1,
                email: "alice@example.com",
                password_hash: hash_password("correct horse battery", &params).unwrap(),
                role: "admin",
            },
            DemoUser {
                id: 2,
                email: "bob@example.com",
                password_hash: hash_password("hunter2hunter2", &params).unwrap(),
                role: "user",
            },
        ]
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 加载并校验配置（`[auth].jwt.secret` 惰性必填）
    let config = preset::load_env_var("AURORA_ENV", "config")?;
    // 2. 初始化日志与追踪
    init_tracing(&config.telemetry)?;
    // 3. 构建应用状态（含 AuthService）
    let state = AppState::build(&config).await?;

    // 3.1 注册 RBAC 策略（演示用；生产通常由 `[authz].policy_path` 加载）
    // 示例的 required-features 含 `authz`，故这里一定装上了授权器。
    let authz = state.auth.authorizer().expect("已启用 authz");
    authz.add_policy("admin", "users", "delete").await?;
    authz.add_role("admin", "alice").await?;
    authz.add_policy("user", "users", "read").await?;

    // 4. 业务路由：**默认受保护**，只有登录接口显式放进 public 组
    let routes = AppRoutes::new()
        .public(|r| r.route("/login", post(login)))
        .protected(|r| {
            r.route("/me", get(me))
                .route("/users/{id}", delete(delete_user))
        });
    // 5. 组装 Router（auth 中间件在最内层解析 Bearer，守卫层校验 Identity）
    let app = build_router(routes, state);
    // 6. 起服务 + 优雅关闭
    let addr = config.server.socket_addr()?;
    tracing::info!(%addr, "auth_guard example starting");
    serve(app, addr).await?;

    tracing::info!("server shut down gracefully");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct LoginReq {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct IdPath {
    id: i64,
}

#[derive(Debug, serde::Serialize)]
struct MeDto {
    sub: String,
    roles: Vec<String>,
    id: Option<i64>,
}

/// 登录：校验口令后签发双令牌。
///
/// 「用户不存在」与「密码错误」**统一返回** `401 invalid credentials`，
/// 避免用户枚举（见 `docs/11` §6）。
async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> AppResult<ApiResponse<TokenPair>> {
    // 用户不存在时也跑一次等价开销的哈希：否则「快 = 账号不存在」，
    // 响应时间本身就是账号枚举器（见 `docs/11` §6）。
    let user = demo_users().iter().find(|u| u.email == req.email);
    let hash = user.map(|u| u.password_hash.as_str());
    if !verify_password_or_dummy(&req.password, hash, state.security.argon2())? {
        return Err(AppError::unauthenticated("invalid credentials"));
    }
    // 口令正确但用户不存在（dummy 校验必 false）时同样走统一提示
    let Some(user) = user else {
        return Err(AppError::unauthenticated("invalid credentials"));
    };

    let principal = Principal::from_id(user.id).with_role(user.role);
    Ok(ApiResponse::ok(state.auth.sign_jwt(&principal)?))
}

/// 当前身份（`Principal` 提取器：无 / 错 token → 401）。
async fn me(principal: Principal) -> AppResult<ApiResponse<MeDto>> {
    Ok(ApiResponse::ok(MeDto {
        sub: principal.sub.clone(),
        roles: principal.roles.clone(),
        id: principal.id(),
    }))
}

/// 受保护写操作：先过 `Principal`，再过 RBAC 权限校验。
async fn delete_user(
    State(state): State<AppState>,
    principal: Principal,
    Path(path): Path<IdPath>,
) -> AppResult<ApiResponse<()>> {
    // 等价于：require_permission!(state.auth, &principal, "users", "delete");
    state.auth.require(&principal, "users", "delete").await?;
    tracing::info!(user = %principal.sub, id = path.id, "user deleted");
    Ok(ApiResponse::ok(()))
}
