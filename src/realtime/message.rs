//! 实时消息结构：event / topic / payload（与 `queue::Message` 一致的
//! serde_json::Value 载荷约定）。

use serde::{Deserialize, Serialize};

/// 实时消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeMessage {
    /// 事件名（前端按 event 分发，如 "notify"、"progress"）
    pub event: String,
    /// 频道 / 房间（如 "user:42"、"video:1001:progress"）
    pub topic: String,
    /// 载荷（JSON 值）
    pub payload: serde_json::Value,
}

impl RealtimeMessage {
    pub fn new(
        event: impl Into<String>,
        topic: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event: event.into(),
            topic: topic.into(),
            payload,
        }
    }

    /// 编码为 JSON 文本（ws 文本帧 / SSE data 字段）
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}
