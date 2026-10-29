//! resilience_upstream —— 出站韧性：重试 / 熔断 / 舱壁 / 降级。
//!
//! 运行：`cargo run --example resilience_upstream --features resilience`
//!
//! 上游用**进程内模拟**（不依赖网络），四种行为：
//!
//! ```bash
//! # 正常
//! curl -s http://127.0.0.1:8093/upstream/ok
//!
//! # 抖动：每 3 次只成功 1 次 —— 重试在 3 次内兜住
//! curl -s http://127.0.0.1:8093/upstream/flaky
//!
//! # 持续失败：连续 3 次调用后熔断打开，后续请求快速失败（50006）
//! for i in 1 2 3 4; do curl -s http://127.0.0.1:8093/upstream/down; echo; done
//!
//! # 超时：上游 500ms，出站超时 200ms → 10006（非幂等，不重试）
//! curl -s http://127.0.0.1:8093/upstream/slow
//!
//! # 降级：主调用失败时返回缓存值
//! curl -s http://127.0.0.1:8093/fallback
//!
//! # 当前熔断状态 / 舱壁剩余名额
//! curl -s http://127.0.0.1:8093/status
//! ```
//!
//! 治理顺序（`PolicyRegistry::execute_scoped`）：**舱壁 → 熔断 → 超时**，
//! 每次**尝试**完整走一遍，失败后按重试策略退避（退避期间不占舱壁名额）。
//! 即：先拒绝超额并发，熔断打开时快速失败，熔断器按每次尝试计数，超时只约束单次尝试。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;

use aurora::http::Path;
use aurora::prelude::*;
use aurora::resilience::Fallback;

/// 模拟上游。
#[derive(Debug, Default)]
struct Upstream {
    /// 累计被调用次数（用于观察重试次数）。
    calls: AtomicU32,
}

impl Upstream {
    /// 按模式返回结果；`flaky` 每 3 次只成功 1 次。
    async fn fetch(&self, mode: &str) -> AppResult<String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        match mode {
            "ok" => Ok(format!("upstream-ok #{call}")),
            "flaky" => {
                if call.is_multiple_of(3) {
                    Ok(format!("upstream-flaky #{call}"))
                } else {
                    Err(AppError::biz(
                        ErrorCode::UPSTREAM_ERROR,
                        format!("上游抖动 #{call}"),
                    ))
                }
            }
            "down" => Err(AppError::biz(ErrorCode::UPSTREAM_ERROR, "上游不可用")),
            "slow" => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok("upstream-slow".to_string())
            }
            other => Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("未知模式：{other}（可选 ok / flaky / down / slow）"),
            )),
        }
    }
}

/// `GET /upstream/{mode}` —— 经 `PolicyRegistry` 调用上游。
async fn call_upstream(
    State(state): State<AppState>,
    axum::Extension(upstream): axum::Extension<Arc<Upstream>>,
    Path(mode): Path<String>,
) -> AppResult<ApiResponse<serde_json::Value>> {
    let registry = state.resilience.clone();
    let before = upstream.calls.load(Ordering::SeqCst);
    let started = Instant::now();

    let result = registry
        .execute_scoped("demo-upstream", || {
            let upstream = Arc::clone(&upstream);
            let mode = mode.clone();
            async move { upstream.fetch(&mode).await }
        })
        .await;

    let attempts = upstream.calls.load(Ordering::SeqCst) - before;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(body) => Ok(ApiResponse::ok(serde_json::json!({
            "mode": mode,
            "body": body,
            "attempts": attempts,
            "elapsed_ms": elapsed_ms,
        }))),
        Err(error) => {
            // 保留原错误码，便于观察 50003（上游失败）/ 50006（熔断打开）/ 50007（舱壁满）/ 10006（超时）
            tracing::warn!(
                mode = %mode,
                code = %error.code(),
                attempts,
                elapsed_ms,
                "上游调用失败"
            );
            Err(error)
        }
    }
}

/// `GET /fallback` —— 主调用失败时返回降级值（不会把错误抛给客户端）。
async fn fallback(
    State(state): State<AppState>,
    axum::Extension(upstream): axum::Extension<Arc<Upstream>>,
) -> AppResult<ApiResponse<serde_json::Value>> {
    let registry = state.resilience.clone();
    let cached = Fallback::new("cached: 上游暂不可用，返回缓存值".to_string());

    // 注意：`Fallback` 在**外层**，因此熔断打开 / 重试耗尽都会被兜住；
    // 但鉴权 / 限流 / 参数类错误不会被兜住——降级只针对「依赖不可用」。
    let body = cached
        .run(|| {
            let registry = registry.clone();
            let upstream = Arc::clone(&upstream);
            async move {
                registry
                    .execute_scoped("demo-upstream", || {
                        let upstream = Arc::clone(&upstream);
                        async move { upstream.fetch("down").await }
                    })
                    .await
            }
        })
        .await?;

    Ok(ApiResponse::ok(serde_json::json!({ "body": body })))
}

/// `GET /status` —— 熔断状态与舱壁余量。
async fn status(State(state): State<AppState>) -> ApiResponse<serde_json::Value> {
    let breaker = state.resilience.circuit_breaker_for("demo-upstream");
    let bulkhead = state.resilience.bulkhead_for("demo-upstream");
    ApiResponse::ok(serde_json::json!({
        "circuit_state": format!("{:?}", breaker.state().await),
        "bulkhead_available": bulkhead.available(),
        "bulkhead_max": bulkhead.max_concurrent(),
        "retry_max_attempts": state.resilience.retry().max_attempts(),
        "retry_idempotent": state.resilience.retry().is_idempotent(),
        "outbound_timeout_ms": state.resilience.timeout().as_millis() as u64,
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = preset::load_env_var("AURORA_ENV", "config").unwrap_or_default();
    config.server.port = 8093;

    // 演示参数：把阈值调小，方便几次请求就能看到熔断
    config.resilience.retry.max_attempts = 3;
    config.resilience.retry.base_delay = Duration::from_millis(30);
    config.resilience.circuit_breaker.failure_threshold = 3;
    config.resilience.circuit_breaker.open_duration = Duration::from_secs(5);
    config.resilience.bulkhead.max_concurrent = 2;
    config.resilience.timeout.default = Duration::from_millis(200);
    config.validate()?;

    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    println!("出站治理参数：");
    println!(
        "  retry.max_attempts       = {}",
        config.resilience.retry.max_attempts
    );
    println!(
        "  retry.base_delay         = {:?}",
        config.resilience.retry.base_delay
    );
    println!(
        "  circuit.failure_threshold= {}",
        config.resilience.circuit_breaker.failure_threshold
    );
    println!(
        "  circuit.open_duration    = {:?}",
        config.resilience.circuit_breaker.open_duration
    );
    println!(
        "  bulkhead.max_concurrent  = {}",
        config.resilience.bulkhead.max_concurrent
    );
    println!(
        "  timeout.default          = {:?}",
        config.resilience.timeout.default
    );

    let upstream = Arc::new(Upstream::default());
    let routes = Router::new()
        .route("/upstream/{mode}", get(call_upstream))
        .route("/fallback", get(fallback))
        .route("/status", get(status))
        .layer(axum::Extension(Arc::clone(&upstream)));

    let app = build_router(AppRoutes::all_public(routes), state);
    let addr = config.server.socket_addr()?;
    println!("\n监听 {addr}");
    serve(app, addr).await?;
    Ok(())
}
