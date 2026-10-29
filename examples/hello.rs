//! hello —— aurora 最小示例：启动 + 路由/提取器 + 健康检查 + 优雅关闭。
//!
//! 运行：`cargo run --example hello`
//!
//! 启动后：
//! - `GET  /`        → 业务路由
//! - `POST /echo`    → aurora 版 JSON 提取器（失败自动映射为统一错误体）
//! - `GET  /health`  → liveness（不访问依赖）
//! - `GET  /ready`   → readiness（执行已注册探针）
//!
//! 每个响应都带 `X-Trace-Id` 头，且与响应体 `trace_id` 一致。
//! 按 `Ctrl-C`（Windows）/ `SIGTERM`（Unix）触发优雅关闭。

use aurora::http::Json;
use aurora::prelude::*;
use aurora::telemetry::FnHealth;
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 加载并校验配置
    let config = preset::load_env_var("AURORA_ENV", "config")?;
    // 2. 初始化日志与追踪
    init_tracing(&config.telemetry)?;
    // 3. 构建应用状态
    let state = AppState::build(&config).await?;
    // 3.1 注册就绪探针（此处为演示用的恒健康探针；实际应探测 db / cache / queue）
    state
        .health
        .register(FnHealth::new("demo", || async { Ok(()) }));
    // 3.2 生命周期钩子：启动末预热 / 关闭前清理（启动按注册顺序，关闭按逆序）
    state.lifecycle.on_startup("demo-warmup", || async {
        tracing::info!("启动钩子：预热完成");
        Ok::<(), String>(())
    });
    state.lifecycle.on_shutdown("demo-flush", || async {
        tracing::info!("关闭钩子：资源已清理");
        Ok::<(), String>(())
    });
    // 启动失败会先逆序回滚已完成的钩子，再把错误交给调用方
    state.lifecycle.run_startup().await?;
    // 4. 业务路由（/health、/ready 由 build_router 自动挂载）
    let routes = Router::new()
        .route("/", get(index))
        .route("/echo", post(echo));
    // 5. 组装 Router（套中间件栈；state 由 Router 与钩子共享）
    let app = build_router(AppRoutes::all_public(routes), state.clone());
    // 6. 起服务 + 优雅关闭
    let addr = config.server.socket_addr()?;
    tracing::info!(%addr, "aurora hello example starting");
    serve(app, addr).await?;

    // 7. 关闭钩子（逆序执行；单个失败只记日志，不阻断其余清理）
    state.lifecycle.run_shutdown().await;
    tracing::info!("server shut down gracefully");
    Ok(())
}

async fn index() -> ApiResponse<&'static str> {
    ApiResponse::ok("hello, aurora")
}

/// 回显请求体；非法 JSON / 缺少 `Content-Type` 会由框架映射为统一错误体。
async fn echo(Json(payload): Json<EchoRequest>) -> ApiResponse<EchoResponse> {
    ApiResponse::ok(EchoResponse {
        name: payload.name,
        trace_id: aurora::telemetry::current_trace_id(),
    })
}

#[derive(Debug, Deserialize)]
struct EchoRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct EchoResponse {
    name: String,
    trace_id: String,
}
