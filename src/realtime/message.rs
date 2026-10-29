//! 实时消息。

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult, ErrorCode};

/// 消息载荷类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    /// 纯文本（WS `Text` / SSE `data`）。
    Text,
    /// JSON（WS `Text`；内容为序列化后的 JSON 串）。
    Json,
    /// 二进制（仅 WS）。
    Binary,
}

/// 广播消息。
///
/// 由业务构造，经 [`Hub::publish`](crate::realtime::Hub::publish) 投递到频道；
/// `topic` 以 `publish` 的入参为准（构造时无需指定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeMessage {
    /// 频道（如 `room:1`）。
    pub topic: String,
    /// 载荷类型。
    pub kind: MessageKind,
    /// 载荷（`Binary` 时为 base64 或业务自定义编码的字符串）。
    pub payload: String,
    /// SSE `event:` 名（`None` 时用默认 `message`）。
    pub event: Option<String>,
    /// 消息 ID（SSE `id:`；便于客户端断线续传去重）。
    pub id: String,
}

impl RealtimeMessage {
    /// 文本消息。
    #[must_use]
    pub fn text(payload: impl Into<String>) -> Self {
        Self::new(MessageKind::Text, payload.into())
    }

    /// JSON 消息（序列化失败 → `10001`）。
    ///
    /// # Errors
    /// 序列化失败时返回 `10001`（`INVALID_PARAM`）。
    pub fn json<T: Serialize>(value: &T) -> AppResult<Self> {
        let payload = serde_json::to_string(value).map_err(|error| {
            AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("实时消息序列化失败：{error}"),
            )
        })?;
        Ok(Self::new(MessageKind::Json, payload))
    }

    /// 二进制消息（仅 WS 生效；SSE 会按文本发送）。
    #[must_use]
    pub fn binary(payload: impl Into<String>) -> Self {
        Self::new(MessageKind::Binary, payload.into())
    }

    /// 指定 SSE 事件名（WS 忽略）。
    #[must_use]
    pub fn with_event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }

    /// 指定消息 ID（SSE `id:`）。
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    fn new(kind: MessageKind, payload: String) -> Self {
        Self {
            topic: String::new(),
            kind,
            payload,
            event: None,
            id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_text_message_with_id() {
        let message = RealtimeMessage::text("hello");
        assert_eq!(message.kind, MessageKind::Text);
        assert_eq!(message.payload, "hello");
        assert_eq!(message.id.len(), 36, "默认用 UUID");
        assert!(message.event.is_none());
    }

    #[test]
    fn should_serialize_json_payload() {
        let message = RealtimeMessage::json(&serde_json::json!({"n": 1})).unwrap();
        assert_eq!(message.kind, MessageKind::Json);
        assert_eq!(message.payload, r#"{"n":1}"#);
    }

    #[test]
    fn should_allow_custom_event_and_id() {
        let message = RealtimeMessage::text("x").with_event("tick").with_id("m-1");
        assert_eq!(message.event.as_deref(), Some("tick"));
        assert_eq!(message.id, "m-1");
    }
}

/// 上行消息（客户端 → 服务端）。
///
/// 由 WebSocket 入站帧构造，交给 [`Hub::on_message`](crate::realtime::Hub::on_message)
/// 注册的处理器；处理器内可 `tokio::spawn` 做异步工作，再通过 `Hub::publish` 广播回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    /// 频道（WS 路径参数）。
    pub topic: String,
    /// 帧类型（`Text` / `Json` / `Binary`）。
    pub kind: MessageKind,
    /// 原始载荷。
    pub payload: Vec<u8>,
}

impl InboundMessage {
    /// 构造上行消息。
    #[must_use]
    pub fn new(topic: impl Into<String>, kind: MessageKind, payload: Vec<u8>) -> Self {
        Self {
            topic: topic.into(),
            kind,
            payload,
        }
    }

    /// 文本视图（非 UTF-8 时返回 `None`）。
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.payload).ok()
    }

    /// 按 JSON 解析载荷。
    ///
    /// # Errors
    /// 载荷不是合法 JSON 时返回 `10001`。
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> AppResult<T> {
        serde_json::from_slice(&self.payload).map_err(|error| {
            AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("上行消息不是合法 JSON：{error}"),
            )
        })
    }
}
