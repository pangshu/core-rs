//! openapi_docs —— M4 示例：utoipa 文档 + 文档 UI。
//!
//! 运行：`cargo run --example openapi_docs --features openapi`
//!
//! 演示：
//! - 业务侧用 `#[derive(OpenApi)]` 声明文档（paths / schemas）
//! - 统一响应体 `ApiResponse<T>` 在文档中**按泛型展开**（`data` 指向业务模型）
//! - `OpenApiRouter` 挂载规范 JSON 端点与文档 UI（路径取自 `[openapi]`）
//!
//! ```bash
//! curl -s http://127.0.0.1:8080/api-docs/openapi.json | head -40
//! # 浏览器打开 http://127.0.0.1:8080/docs
//! curl -s http://127.0.0.1:8080/users
//! curl -s -i http://127.0.0.1:8080/users/2      # 404 / 30001
//! ```
//!
//! 说明：业务侧若在自己的 crate 中使用 `#[derive(OpenApi)]`，需自行把 `utoipa`
//! 加入依赖（`utoipa` 5，开启默认的 `macros` feature）。

use aurora::http::Path;
use aurora::openapi::OpenApiRouter;
use aurora::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};

/// 业务模型（由业务侧声明；框架不内置实体）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
struct User {
    id: i64,
    email: String,
}

/// 汇总本应用的文档：路径 + 组件 schema。
#[derive(OpenApi)]
#[openapi(
    info(title = "aurora openapi_docs example", version = "0.1.0"),
    paths(list_users, get_user),
    components(schemas(User))
)]
struct ApiDoc;

#[utoipa::path(
    get,
    path = "/users",
    responses((status = 200, description = "用户列表", body = ApiResponse<Vec<User>>))
)]
async fn list_users() -> AppResult<ApiResponse<Vec<User>>> {
    Ok(ApiResponse::ok(vec![User {
        id: 1,
        email: "a@b.com".to_string(),
    }]))
}

#[utoipa::path(
    get,
    path = "/users/{id}",
    params(("id" = i64, Path, description = "用户 ID")),
    responses(
        (status = 200, description = "用户详情", body = ApiResponse<User>),
        // 注：错误响应体不写 `body`——`ApiResponse<()>` 的 `()` 在 utoipa 宏里没有类型路径。
        (status = 404, description = "资源不存在（30001）")
    )
)]
async fn get_user(Path(id): Path<i64>) -> AppResult<ApiResponse<User>> {
    if id == 1 {
        Ok(ApiResponse::ok(User {
            id,
            email: "a@b.com".to_string(),
        }))
    } else {
        Err(AppError::not_found(format!("user {id}")))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 加载并校验配置
    let mut config = preset::load_env_var("AURORA_ENV", "config")?;
    // `[openapi].enabled` 默认关闭（避免生产暴露路由清单），示例里显式打开
    config.openapi.enabled = true;
    // 2. 初始化日志与追踪
    init_tracing(&config.telemetry)?;
    // 3. 构建应用状态
    let state = AppState::build(&config).await?;
    // 4. 业务路由 + 文档路由（规范 JSON 端点与 UI）
    let docs = OpenApiRouter::new().merge::<ApiDoc>();
    let routes = Router::new()
        .route("/users", get(list_users))
        .route("/users/{id}", get(get_user))
        .merge(docs.into_routes(&config.openapi));
    // 5. 组装 Router（套中间件栈；/health、/ready 由 build_router 自动挂载）
    let app = build_router(AppRoutes::all_public(routes), state);
    // 6. 起服务 + 优雅关闭
    let addr = config.server.socket_addr()?;
    tracing::info!(
        %addr,
        docs = %config.openapi.path,
        spec = %config.openapi.json_path,
        "openapi_docs example starting"
    );
    serve(app, addr).await?;

    tracing::info!("server shut down gracefully");
    Ok(())
}
