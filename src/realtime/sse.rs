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

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};

use crate::realtime::hub::Hub;
use crate::realtime::RealtimeMessage;

/// 订阅 `topic` 频道并返回 SSE 流（断开即注销，无残留连接）
pub async fn handle<S>(state: S, topic: String) -> Response
where
    S: crate::traits::HasRealtime + Send + Sync + 'static,
{
    stream(state.hub().clone(), topic).await
}

/// 直接基于 hub 构造 SSE 响应
pub async fn stream(hub: Arc<Hub>, topic: String) -> Response {
    let (conn_id, downstream) = match hub.register(&topic) {
        Ok(x) => x,
        Err(e) => {
            // 容量满：503
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(crate::web::response::ApiResponse::error(
                    503,
                    format!("server at capacity ({e})"),
                )),
            )
                .into_response();
        }
    };

    // 断开清理：SSE 流结束（客户端断开 → next() 返回 None → body drop）
    struct Cleanup {
        hub: Arc<Hub>,
        topic: String,
        conn_id: usize,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.hub.unregister(&self.topic, self.conn_id);
        }
    }
    let cleanup = Cleanup {
        hub: hub.clone(),
        topic: topic.clone(),
        conn_id,
    };

    let keep_alive = Duration::from_secs(hub.heartbeat_secs());
    let sse_body = Sse::new(futures::stream::unfold(
        (downstream, cleanup),
        |(mut rx, cleanup)| async move {
            match rx.recv().await {
                Some(msg) => Some((to_event(&msg), (rx, cleanup))),
                None => None, // hub 关闭该连接：流结束（触发 Cleanup drop）
            }
        },
    ))
    .keep_alive(KeepAlive::new().interval(keep_alive).text("ping"));

    sse_body.into_response()
}

fn to_event(msg: &RealtimeMessage) -> Result<Event, Infallible> {
    Ok(Event::default()
        .event(msg.event.clone())
        .data(msg.payload.to_string()))
}
