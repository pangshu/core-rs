//! SSE 端点（feature `sse`）。
//!
//! 与 WS 共用 [`Hub`]：订阅同一频道即可同时服务两种客户端。
//! 建流顺序与 WS 一致：`频道名校验 → 订阅授权 → 连接数上限 → 订阅`。
//! 心跳由 `KeepAlive` 承担（`[realtime].heartbeat_interval`），
//! 关闭时 `Hub::shutdown()` 会让流自然结束。

use std::borrow::Cow;
use std::convert::Infallible;

use axum::extract::{Query, Request};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::future::BoxFuture;
use futures_util::stream::unfold;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::realtime::hub::{Hub, SubscribeRequest};
use crate::realtime::message::RealtimeMessage;
use crate::realtime::topic;
use crate::utils::ClientIp;

/// 连续落后多少次就断开。
///
/// 订阅者持续 `Lagged` 说明它已经跟不上这个频道：继续留着既占连接名额，又让客户端
/// 拿到残缺的流（静默跳消息）。断开后由客户端重连走「快照 + 增量」是更诚实的选择。
const MAX_CONSECUTIVE_LAG: u32 = 64;

/// SSE 查询参数。
#[derive(Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct StreamQuery {
    /// 频道名（缺省 `default`）。
    pub topic: Option<String>,
}

/// 构造 SSE 处理函数（路由如 `/events?topic=room:1`）。
///
/// ```no_run
/// use aurora::config::AppConfig;
/// use aurora::realtime::{Hub, sse};
///
/// let config = AppConfig::default();
/// let hub = Hub::from_config(&config.realtime);
/// hub.on_subscribe(|req| req.topic == "public:tick");
/// let _stream = sse::stream(hub);
/// ```
pub fn stream(
    hub: Hub,
) -> impl Fn(Query<StreamQuery>, ClientIp, Request) -> BoxFuture<'static, Response>
+ Clone
+ Send
+ Sync
+ 'static {
    move |Query(query): Query<StreamQuery>, ClientIp(ip): ClientIp, request: Request| {
        let hub = hub.clone();
        Box::pin(async move {
            let topic = query.topic.unwrap_or_else(|| "default".to_string());

            if let Err(error) = topic::validate(&topic, hub.config().max_topic_len) {
                tracing::warn!(
                    topic = %topic::log_view(&topic),
                    "SSE 频道名非法，拒绝建流"
                );
                return error.into_response();
            }
            // 授权在占用名额之前：未授权请求不该消耗连接配额
            if let Err(error) = hub.authorize_subscribe(&SubscribeRequest {
                topic: &topic,
                peer: ip,
                headers: request.headers(),
                extensions: request.extensions(),
            }) {
                tracing::warn!(topic = %topic::log_view(&topic), "SSE 订阅被拒绝");
                return error.into_response();
            }

            // 连接名额随流存活（stream 结束时 drop 归还）。
            let subject = crate::realtime::hub::subject_from_extensions(request.extensions());
            let guard = match hub.acquire_connection_for(ip, subject.as_deref()) {
                Ok(guard) => guard,
                Err(error) => return error.into_response(),
            };
            let subscription = match hub.subscribe(&topic) {
                Ok(subscription) => subscription,
                Err(error) => return error.into_response(),
            };

            let interval = hub.config().heartbeat_interval;
            let events = unfold(
                (subscription, guard, 0_u32),
                |(mut subscription, guard, mut lagged)| async move {
                    loop {
                        match subscription.recv().await {
                            Ok(message) => {
                                let event = to_event(&message);
                                return Some((
                                    Ok::<_, Infallible>(event),
                                    (subscription, guard, 0),
                                ));
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                lagged += 1;
                                tracing::warn!(
                                    skipped,
                                    consecutive = lagged,
                                    "SSE 订阅端落后，已跳过部分消息"
                                );
                                if lagged >= MAX_CONSECUTIVE_LAG {
                                    tracing::warn!(
                                        "SSE 订阅端持续落后，主动结束该流（请重连后走快照 + 增量）"
                                    );
                                    return None;
                                }
                            }
                            // Hub::shutdown() → 结束流
                            Err(broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                },
            );

            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(interval))
                .into_response()
        })
    }
}

/// 广播消息 → SSE 事件（`id` / `event` / `data`）。
///
/// `data` 由 axum 按行加 `data: ` 前缀，天然安全；但 `id` / `event` 是**直接拼**进
/// 帧里的，且 axum 0.8 对二者含 `\r`/`\n` 的处理是 `assert` **panic**——一条带换行的
/// 消息会把该频道下**所有** SSE 订阅者的响应流打断（`CatchPanicLayer` 只覆盖
/// handler，覆盖不到 body 流）。因此在构造前剔除控制字符。
fn to_event(message: &RealtimeMessage) -> Event {
    let id = sanitize_field(&message.id);
    let mut event = Event::default()
        .id(id.as_ref())
        .data(message.payload.clone());
    if let Some(name) = &message.event {
        let name = sanitize_field(name);
        event = event.event(name.as_ref());
    }
    event
}

/// 剔除控制字符（`\r` / `\n` 会让 axum panic，其余控制字符不符合 SSE 规范）。
fn sanitize_field(value: &str) -> Cow<'_, str> {
    if !value.chars().any(char::is_control) {
        return Cow::Borrowed(value);
    }
    Cow::Owned(value.chars().filter(|c| !c.is_control()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realtime::message::MessageKind;

    #[test]
    fn should_build_event_from_message() {
        let message = RealtimeMessage::text("payload")
            .with_event("tick")
            .with_id("m1");
        let event = to_event(&message);
        let rendered = format!("{event:?}");
        assert!(rendered.contains("tick"), "应带事件名：{rendered}");
        assert!(rendered.contains("m1"), "应带消息 id：{rendered}");
    }

    #[test]
    fn should_not_panic_on_control_chars() {
        // axum 0.8 的 `Event::field` 对 `\r`/`\n` 是 assert panic——这里必须被剔除，
        // 否则一条消息会把该频道下所有 SSE 订阅者的响应流打断
        let message = RealtimeMessage::text("payload")
            .with_event("tick\r\n\r\ndata: forged")
            .with_id("m1\nid: forged");
        let rendered = format!("{:?}", to_event(&message));
        assert!(
            rendered.contains("tickdata: forged"),
            "换行应被剔除：{rendered}"
        );
        assert!(
            rendered.contains("m1id: forged"),
            "换行应被剔除：{rendered}"
        );
    }

    #[test]
    fn should_keep_plain_fields_borrowed() {
        assert!(matches!(sanitize_field("tick"), Cow::Borrowed(_)));
        assert!(matches!(sanitize_field("a\nb"), Cow::Owned(_)));
        assert_eq!(sanitize_field("a\r\nb"), "ab");
    }

    #[test]
    fn should_default_query_topic() {
        let query = StreamQuery::default();
        assert!(query.topic.is_none());
        assert_eq!(MessageKind::Json, MessageKind::Json);
    }
}
