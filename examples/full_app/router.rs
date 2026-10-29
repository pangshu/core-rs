//! 路由装配。
//!
//! `/health`、`/ready`（框架自带）与 `/metrics`（`[telemetry].metrics.enabled`）
//! 由 `build_router` 挂载，这里只写业务路由。
//!
//! 分组原则（`AppRoutes` 默认受保护）：
//! - **public**：登录（还没有令牌）、实时端点（自带 Origin / 订阅授权校验）；
//! - **protected**：其余业务接口，无有效 `Identity` 一律 401。

use std::sync::Arc;

use axum::Extension;

use aurora::prelude::*;
use aurora::realtime::{sse, websocket};

use crate::features;
use crate::state::DemoCtx;

/// 业务路由（含 SSE / WS 端点）。
pub fn api(ctx: Arc<DemoCtx>) -> AppRoutes {
    let hub = ctx.hub.clone();
    // 业务上下文注入（`Extension<Arc<DemoCtx>>`）对两组都要生效
    let with_ctx = |router: Router<AppState>| router.layer(Extension(Arc::clone(&ctx)));

    let public = with_ctx(
        Router::new()
            .route("/api/v1/auth/login", post(features::auth::login))
            // 实时：同一频道 `todos`，SSE 与 WS 客户端都能收到。
            // 鉴权在 `hub.on_subscribe` 里做（未注册即拒绝），故不需要 Identity。
            .route("/events", get(sse::stream(hub.clone())))
            .route("/ws/{topic}", get(websocket::handler(hub))),
    );

    let protected = with_ctx(
        Router::new()
            .route("/api/v1/me", get(features::auth::me))
            .route(
                "/api/v1/todos",
                get(features::todos::list).post(features::todos::create),
            )
            .route(
                "/api/v1/todos/{id}",
                get(features::todos::get_one).delete(features::todos::delete),
            ),
    );

    AppRoutes::all_public(public).protected(|_| protected)
}
