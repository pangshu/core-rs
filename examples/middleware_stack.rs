//! middleware_stack —— M5 示例：内置中间件栈与装配顺序。
//!
//! 运行：`cargo run --example middleware_stack --features "security-headers,ip-filter,idempotency"`
//!
//! ```bash
//! # 1) 幂等：同一个 Idempotency-Key 重复提交只会产生一个订单
//! curl -s -X POST -H 'Idempotency-Key: demo-1' http://127.0.0.1:8080/orders
//! curl -s -X POST -H 'Idempotency-Key: demo-1' http://127.0.0.1:8080/orders   # 回放（Idempotent-Replay: true）
//! curl -s -X POST -H 'Idempotency-Key: demo-2' http://127.0.0.1:8080/orders   # 新订单
//!
//! # 2) IP 过滤：命中黑名单直接 40002（429）
//! curl -si http://127.0.0.1:8080/whoami
//!
//! # 3) 安全响应头 / 请求 ID 等由 `build_router` 自动挂载
//! curl -si http://127.0.0.1:8080/whoami | grep -i 'x-content-type-options\|x-request-id'
//! ```
//!
//! **装配顺序**（由外到内，见 `docs/02` §6）：
//! `panic → request_id → trace → access_log → security_headers → envelope(413) →
//! body_limit → envelope(408) → timeout → ip_filter → cors → compression →
//! locale → csrf → rate_limit → auth → idempotency`
//!
//! 顺序由 `build_router` 统一保证，应用**不需要**手工 `.layer()`——手工装配极易
//! 写反（`Router::layer` 是「后添加=外层」，与 `ServiceBuilder` 相反）。
//!
//! CSRF（双提交 Cookie）不在本示例的 required-features 内；需要时把 `csrf` 加进
//! feature 列表并设置 `[http].csrf.enabled = true` 即可。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::routing::{get, post};

use aurora::config::IpFilterMode;
use aurora::prelude::*;
use aurora::utils::client_ip::ClientIp;

async fn whoami(client_ip: ClientIp) -> AppResult<ApiResponse<String>> {
    let ip = client_ip
        .0
        .map_or_else(|| "unknown".to_string(), |ip| ip.to_string());
    Ok(ApiResponse::ok(ip))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = preset::load_env_var("AURORA_ENV", "config")?;

    // IP 黑名单：命中即 40002（429）。本机 127.0.0.1 不在名单内，可直接 curl。
    config.http.ip_filter.mode = IpFilterMode::Deny;
    config.http.ip_filter.cidrs = vec!["10.0.0.0/8".parse()?];

    // 幂等：写操作带 `Idempotency-Key` 时回放首次结果
    config.http.idempotency.enabled = true;

    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    let seq = Arc::new(AtomicU32::new(0));
    let counter = seq.clone();
    let routes = Router::new().route("/whoami", get(whoami)).route(
        "/orders",
        post(move || {
            let counter = counter.clone();
            async move {
                let id = counter.fetch_add(1, Ordering::SeqCst) + 1;
                ApiResponse::ok(format!("order-{id}"))
            }
        }),
    );

    let app = build_router(AppRoutes::all_public(routes), state);

    let addr = config.server.socket_addr()?;
    tracing::info!(
        %addr,
        ip_filter = config.http.ip_filter.mode.as_str(),
        idempotency = config.http.idempotency.enabled,
        "middleware_stack example starting"
    );
    serve(app, addr).await?;
    Ok(())
}
