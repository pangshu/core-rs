//! 统一消息结构（文档 三·12）：id / topic / payload / headers / 重试计数。
//! 跨后端只保证 JSON 兼容类型不变；与 `realtime::RealtimeMessage` 保持一致的
//! 序列化约定（serde_json::Value 载荷）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 队列消息：`payload` 为 JSON 值载荷
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// 队列分配的消息 ID（redis = stream entry id；memory = 进程内自增序号）
    pub id: String,
    /// 路由主题
    pub topic: String,
    /// 载荷（JSON 值）
    pub payload: serde_json::Value,
    /// 透传头（跨后端保留；redis 存 JSON 字符串字段）
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 投递计数，从 1 起（Worker 重试会递增）
    pub attempts: u32,
}

impl Message {
    pub fn new(topic: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            id: crate::utils::new_id(),
            topic: topic.into(),
            payload,
            headers: BTreeMap::new(),
            attempts: 1,
        }
    }
}
