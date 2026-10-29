//! 队列消息（`id` 即消费幂等键）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

///
/// `id` 同时作为**消费幂等键**（见 `docs/03` §4.12）。
///
/// `Serialize` / `Deserialize` 供 `queue-redis` / `queue-amqp` / `queue-kafka`
/// 以 JSON 编码消息体（`memory` 后端直接用结构体，无编解码开销）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// 消息 ID（全局唯一，幂等键）。
    pub id: String,
    /// 所属队列。
    pub queue: String,
    /// 消息体（业务自定义编码）。
    pub payload: Vec<u8>,
    /// 已投递次数（首次投递为 `0`，每次 `nack` +1）。
    pub attempts: u32,
    /// 入队时间（UTC）。
    pub enqueued_at: DateTime<Utc>,
}

impl Message {
    /// 构造一条新消息（自动生成 `id` 与入队时间）。
    #[must_use]
    pub fn new(queue: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            queue: queue.into(),
            payload,
            attempts: 0,
            enqueued_at: Utc::now(),
        }
    }
}
