//! WebSocket（feature = "ws"，文档 三·15）：axum ws 升级、心跳 ping/pong、
//! 房间/频道、断线清理。
//!
//! 用法：应用路由上直接挂处理器——
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # use axum::extract::{Path, WebSocketUpgrade, State};
//! async fn ws_handler<S: HasRealtime + Send + Sync + 'static>(
//!     State(state): State<S>, Path(topic): Path<String>, ws: WebSocketUpgrade,
//! ) -> axum::response::Response {
//!     core_rs::realtime::websocket::handle(state, ws, topic).await
//! }
//! ```
//!
//! 协议约定：服务端下行文本帧为 `RealtimeMessage` JSON；客户端上行
//! `{"event": "subscribe", "payload": {"topic": "other:topic"}}` 可切换频道。

use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;

use crate::realtime::hub::Hub;

/// 升级 WebSocket 并把连接注册到 `topic` 频道
pub async fn upgrade(hub: Arc<Hub>, topic: String, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move { run_connection(hub, topic, socket).await })
}

/// 便捷封装：`S: HasRealtime` 的路由处理器直接调用
pub async fn handle<S>(state: S, ws: WebSocketUpgrade, topic: String) -> Response
where
    S: crate::traits::HasRealtime + Send + Sync + 'static,
{
    upgrade(state.hub().clone(), topic, ws).await
}

/// 单连接生命周期：hub 注册 → 下行转发 → 上行解析（订阅切换）→ 断线清理
async fn run_connection(hub: Arc<Hub>, initial_topic: String, mut socket: WebSocket) {
    let mut topic = initial_topic;
    let heartbeat_secs = hub.heartbeat_secs();
    let (conn_id, mut downstream) = match hub.register(&topic) {
        Ok(x) => x,
        Err(_) => {
            // 容量满：礼貌拒绝后关闭
            let _ = socket
                .send(WsMessage::Text(
                    serde_json::json!({"event": "error", "payload": "server at capacity"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    tracing::debug!(conn_id, topic = %topic, "ws connected");

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(heartbeat_secs));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // hub → 客户端
            Some(msg) = downstream.recv() => {
                if socket.send(WsMessage::Text(msg.to_json().into())).await.is_err() {
                    break;
                }
            }
            // 心跳（ws 协议层 Ping，客户端自动 Pong）
            _ = heartbeat.tick() => {
                if socket.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            // 客户端 → hub（订阅切换 / 关闭）
            frame = socket.recv() => {
                match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Some(new_topic) = parse_subscribe(&text) {
                            if new_topic != topic && hub.switch(&topic, &new_topic, conn_id) {
                                topic = new_topic;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {} // Pong / Binary / Ping 忽略
                    Some(Err(_)) => break,
                }
            }
        }
    }

    hub.unregister(&topic, conn_id);
    tracing::debug!(conn_id, topic = %topic, "ws disconnected");
}

/// 上行 `{"event": "subscribe", "payload": {"topic": "..."}}` → 目标频道
fn parse_subscribe(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Up {
        event: String,
        #[serde(default)]
        payload: serde_json::Value,
    }
    let up: Up = serde_json::from_str(text).ok()?;
    if up.event != "subscribe" {
        return None;
    }
    let topic = up
        .payload
        .get("topic")
        .and_then(|v| v.as_str())?
        .to_string();
    if topic.is_empty() {
        None
    } else {
        Some(topic)
    }
}
