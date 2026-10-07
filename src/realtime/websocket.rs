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

use std::sync::Arc; // 引入 Arc，用于跨任务共享 Hub

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade}; // 引入 axum ws 提取器与帧类型（别名避免与消息类型冲突）
use axum::response::Response; // 引入 axum 响应类型

use crate::realtime::hub::Hub; // 引入实时通信中心 Hub

/// 升级 WebSocket 并把连接注册到 `topic` 频道
pub async fn upgrade(hub: Arc<Hub>, topic: String, ws: WebSocketUpgrade) -> Response { // 执行协议升级并托管连接
    ws.on_upgrade(move |socket| async move { run_connection(hub, topic, socket).await }) // 升级成功后进入单连接生命周期
}

/// 便捷封装：`S: HasRealtime` 的路由处理器直接调用
pub async fn handle<S>(state: S, ws: WebSocketUpgrade, topic: String) -> Response // 从状态取 hub 并升级连接
where // 泛型约束子句
    S: crate::traits::HasRealtime + Send + Sync + 'static, // 状态需能提供 hub 且可跨任务共享
{
    upgrade(state.hub().clone(), topic, ws).await // 克隆 hub 的 Arc 后调用 upgrade
}

/// 单连接生命周期：hub 注册 → 下行转发 → 上行解析（订阅切换）→ 断线清理
async fn run_connection(hub: Arc<Hub>, initial_topic: String, mut socket: WebSocket) { // 驱动单个 ws 连接的收发循环
    let mut topic = initial_topic; // 当前所在频道（可随订阅切换更新）
    let heartbeat_secs = hub.heartbeat_secs(); // 读取心跳间隔
    let (conn_id, mut downstream) = match hub.register(&topic) { // 注册连接，失败则礼貌拒绝
        Ok(x) => x, // 注册成功：拿到连接 ID 与下行接收端
        Err(_) => { // 注册失败（容量满）
            // 容量满：礼貌拒绝后关闭
            let _ = socket // 忽略发送结果，无论如何都要关闭
                .send(WsMessage::Text( // 发送文本错误帧
                    serde_json::json!({"event": "error", "payload": "server at capacity"}) // 构造错误事件 JSON
                        .to_string() // 序列化为字符串
                        .into(), // 转为 ws 文本帧载荷
                ))
                .await; // 等待发送完成
            return; // 结束连接任务
        }
    };
    tracing::debug!(conn_id, topic = %topic, "ws connected"); // 记录连接建立日志

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(heartbeat_secs)); // 创建心跳定时器
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay); // 错过节拍时延迟而非追赶，避免心跳风暴

    loop { // 连接收发主循环
        tokio::select! { // 并发等待下行、心跳、上行三路事件
            // hub → 客户端
            Some(msg) = downstream.recv() => { // 收到 hub 投递的消息
                if socket.send(WsMessage::Text(msg.to_json().into())).await.is_err() { // 下行发送文本帧
                    break; // 发送失败说明连接已断，退出循环
                }
            }
            // 心跳（ws 协议层 Ping，客户端自动 Pong）
            _ = heartbeat.tick() => { // 心跳定时触发
                if socket.send(WsMessage::Ping(Vec::new().into())).await.is_err() { // 发送协议层 Ping 帧
                    break; // 发送失败则退出循环
                }
            }
            // 客户端 → hub（订阅切换 / 关闭）
            frame = socket.recv() => { // 收到客户端上行帧
                match frame { // 按帧类型分派处理
                    Some(Ok(WsMessage::Text(text))) => { // 文本帧：可能是订阅指令
                        if let Some(new_topic) = parse_subscribe(&text) { // 解析订阅目标频道
                            if new_topic != topic && hub.switch(&topic, &new_topic, conn_id) { // 频道不同且迁移成功
                                topic = new_topic; // 更新当前频道
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break, // 收到关闭帧或流结束则退出
                    Some(Ok(_)) => {} // Pong / Binary / Ping 忽略
                    Some(Err(_)) => break, // 读取出错（连接异常）则退出
                }
            }
        }
    }

    hub.unregister(&topic, conn_id); // 断线清理：从频道注销连接并递减计数
    tracing::debug!(conn_id, topic = %topic, "ws disconnected"); // 记录连接断开日志
}

/// 上行 `{"event": "subscribe", "payload": {"topic": "..."}}` → 目标频道
fn parse_subscribe(text: &str) -> Option<String> { // 解析订阅指令，返回目标频道名
    #[derive(serde::Deserialize)] // 派生反序列化，用于解析上行 JSON
    struct Up { // 上行消息的临时结构
        event: String, // 事件名（期望为 "subscribe"）
        #[serde(default)] // payload 缺省时用 Value::Null
        payload: serde_json::Value, // 载荷（其中可能含 topic）
    }
    let up: Up = serde_json::from_str(text).ok()?; // 解析失败直接返回 None
    if up.event != "subscribe" { // 非订阅事件忽略
        return None; // 返回 None
    }
    let topic = up // 从载荷中提取频道名
        .payload // 取 payload
        .get("topic") // 取 topic 字段
        .and_then(|v| v.as_str())? // 必须是字符串，否则返回 None
        .to_string(); // 转为拥有所有权的 String
    if topic.is_empty() { // 空频道名无效
        None // 返回 None
    } else { // 合法频道名
        Some(topic) // 返回解析结果
    }
}
