//! Server-Sent Events（feature = "sse"，文档 三·15）：服务端单向推送——
//! 通知、进度、feed 流；基于 axum `Sse`，频道语义与 WebSocket 共用 [`Hub`]。
//!
//! 用法：
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # use axum::extract::{Path, State};
//! async fn sse_handler<S: HasRealtime + Send + Sync + 'static>(
//!     State(state): State<S>, Path(topic): Path<String>,
//! ) -> axum::response::Response {
//!     core_rs::realtime::sse::handle(state, topic).await
//! }
//! ```

use std::convert::Infallible; // 引入 Infallible，表示 SSE 事件构造不会失败
use std::sync::Arc; // 引入 Arc，用于跨流共享 Hub
use std::time::Duration; // 引入 Duration，用于 KeepAlive 间隔

use axum::response::sse::{Event, KeepAlive, Sse}; // 引入 axum SSE 事件、保活与响应构造器
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型

use crate::realtime::hub::Hub; // 引入实时通信中心 Hub
use crate::realtime::RealtimeMessage; // 引入实时消息类型

/// 订阅 `topic` 频道并返回 SSE 流（断开即注销，无残留连接）
pub async fn handle<S>(state: S, topic: String) -> Response // 从状态取 hub 并建立 SSE 流
where // 泛型约束子句
    S: crate::traits::HasRealtime + Send + Sync + 'static, // 状态需能提供 hub 且可跨任务共享
{
    stream(state.hub().clone(), topic).await // 克隆 hub 的 Arc 后调用 stream
}

/// 直接基于 hub 构造 SSE 响应
pub async fn stream(hub: Arc<Hub>, topic: String) -> Response { // 注册连接并返回 SSE 响应体
    let (conn_id, downstream) = match hub.register(&topic) { // 注册连接，失败则返回 503
        Ok(x) => x, // 注册成功：拿到连接 ID 与下行接收端
        Err(e) => { // 注册失败（容量满）
            // 容量满：503
            return ( // 构造并返回错误响应
                axum::http::StatusCode::SERVICE_UNAVAILABLE, // HTTP 503 服务不可用
                axum::Json(crate::web::response::ApiResponse::error( // 包装为统一错误响应体
                    503, // 业务错误码
                    format!("server at capacity ({e})"), // 错误信息含容量详情
                )),
            )
                .into_response(); // 转换为 axum 响应
        }
    };

    // 断开清理：SSE 流结束（客户端断开 → next() 返回 None → body drop）
    struct Cleanup { // 守卫结构：流结束时自动注销连接
        hub: Arc<Hub>, // 持有 hub 以执行注销
        topic: String, // 连接所在频道
        conn_id: usize, // 连接 ID
    }
    impl Drop for Cleanup { // 实现 Drop 以在流结束/丢弃时清理
        fn drop(&mut self) { // 丢弃回调
            self.hub.unregister(&self.topic, self.conn_id); // 注销连接并递减在线计数
        }
    }
    let cleanup = Cleanup { // 创建清理守卫，随流一起被持有
        hub: hub.clone(), // 克隆 hub 的 Arc
        topic: topic.clone(), // 克隆频道名
        conn_id, // 连接 ID
    };

    let keep_alive = Duration::from_secs(hub.heartbeat_secs()); // 保活间隔取心跳配置
    let sse_body = Sse::new(futures::stream::unfold( // 用 unfold 把接收端转为 SSE 事件流
        (downstream, cleanup), // 流状态：下行接收端 + 清理守卫
        |(mut rx, cleanup)| async move { // 每次拉取一个事件的异步闭包
            match rx.recv().await { // 等待下一条消息
                Some(msg) => Some((to_event(&msg), (rx, cleanup))), // 有消息：转为事件并回填状态
                None => None, // hub 关闭该连接：流结束（触发 Cleanup drop）
            }
        },
    ))
    .keep_alive(KeepAlive::new().interval(keep_alive).text("ping")); // 设置保活注释帧，防中间层断连

    sse_body.into_response() // 转换为 axum 响应返回
}

fn to_event(msg: &RealtimeMessage) -> Result<Event, Infallible> { // 把实时消息转为 SSE 事件
    Ok(Event::default() // 构造默认 SSE 事件
        .event(msg.event.clone()) // 设置事件名（前端据此分发）
        .data(msg.payload.to_string())) // 设置 data 字段为载荷 JSON 文本
}
