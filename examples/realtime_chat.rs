//! realtime_chat —— M5 示例：WebSocket / SSE 频道广播。
//!
//! 运行：`cargo run --example realtime_chat --features ws,sse`
//!
//! ```bash
//! # SSE（curl 持续输出）
//! curl -N http://127.0.0.1:8080/events?topic=room:1
//!
//! # WebSocket（浏览器控制台）
//! # const ws = new WebSocket("ws://127.0.0.1:8080/ws/room:1");
//! # ws.onmessage = (e) => console.log(e.data);
//! ```
//!
//! 演示：`Hub` 按频道广播；WS 与 SSE **共用同一 Hub**（同一频道两种客户端都能收到）；
//! 后台每秒发布一条 tick；**上行消息**（客户端 → 服务端）经 `Hub::on_message` 注册的
//! 处理器回显到同频道；连接数、消息大小、心跳超时由 `[realtime]` 治理。

use std::time::Duration;

use aurora::prelude::*;
use aurora::realtime::{Hub, RealtimeMessage, handle};

/// 频道名（示例固定一个房间）。
const ROOM: &str = "room:1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = preset::load_env_var("AURORA_ENV", "config")?;
    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    let hub = Hub::from_config(&config.realtime);

    // 订阅授权（**必须注册**：未注册时端点为 fail-closed，一律拒绝）。
    // 示例只有一个公开房间，因此放行全部频道；生产里必须校验频道归属
    // （如 `req.topic == format!("user:{}", 当前用户 id)`）。
    hub.on_subscribe(|_req| true);

    // 上行消息回显：客户端发的文本 → 广播回同频道（含来源频道）
    {
        let publisher = hub.clone();
        hub.on_message(move |inbound| {
            let Ok(text) = std::str::from_utf8(&inbound.payload) else {
                return;
            };
            let payload = serde_json::json!({ "from": inbound.topic, "text": text });
            match RealtimeMessage::json(&payload) {
                Ok(message) => {
                    if let Err(error) = publisher.publish(&inbound.topic, message) {
                        tracing::warn!(%error, "回显失败");
                    }
                }
                Err(error) => tracing::warn!(%error, "回显消息序列化失败"),
            }
        });
    }

    // 后台每秒广播一条 tick（演示用；真实业务在 handler/任务里 publish）
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            let mut tick = 0_u64;
            loop {
                ticker.tick().await;
                if hub.is_closed() {
                    break;
                }
                tick += 1;
                let message =
                    RealtimeMessage::json(&serde_json::json!({ "tick": tick, "room": ROOM }))
                        .expect("序列化 tick 失败");
                if let Err(error) = hub.publish(ROOM, message) {
                    tracing::warn!(%error, "广播失败，停止 tick");
                    break;
                }
            }
        });
    }

    // WS 与 SSE 共用同一个 Hub（`handle` 一次性挂载 /ws/{topic} 与 /events）
    let routes = handle(hub.clone());
    let app = build_router(AppRoutes::all_public(routes), state);

    let addr = config.server.socket_addr()?;
    tracing::info!(
        %addr,
        room = ROOM,
        max_conn = config.realtime.max_connections,
        "realtime_chat example starting"
    );

    serve(app, addr).await?;

    // 优雅关闭：拒绝新连接，并让存量 WS 收到 Close(1001) / SSE 结束流；
    // `shutdown_and_wait` 会等长连接退出（上限 `[realtime].shutdown_timeout`）
    hub.shutdown_and_wait().await;
    tracing::info!("server shut down gracefully");
    Ok(())
}
