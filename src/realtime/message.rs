//! 实时消息结构：event / topic / payload（与 `queue::Message` 一致的
//! serde_json::Value 载荷约定）。

use serde::{Deserialize, Serialize}; // 引入序列化/反序列化派生宏

/// 实时消息
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试、克隆与 serde 编解码能力
pub struct RealtimeMessage { // 定义实时消息结构体：一条广播到频道的最小单元
    /// 事件名（前端按 event 分发，如 "notify"、"progress"）
    pub event: String, // 事件名，前端据此分发处理逻辑
    /// 频道 / 房间（如 "user:42"、"video:1001:progress"）
    pub topic: String, // 频道/房间标识，决定消息投递给哪些连接
    /// 载荷（JSON 值）
    pub payload: serde_json::Value, // 消息载荷，任意 JSON 值（与队列消息约定一致）
}

impl RealtimeMessage { // 为 RealtimeMessage 实现构造与编码方法
    pub fn new( // 构造一条实时消息的便捷函数
        event: impl Into<String>, // 事件名，接受任何可转为 String 的类型
        topic: impl Into<String>, // 频道名，接受任何可转为 String 的类型
        payload: serde_json::Value, // 载荷 JSON 值
    ) -> Self { // 返回构造出的 RealtimeMessage
        Self { // 组装结构体字面量
            event: event.into(), // 把 event 转为 String
            topic: topic.into(), // 把 topic 转为 String
            payload, // 直接存入载荷字段（字段名与变量同名，简写）
        }
    }

    /// 编码为 JSON 文本（ws 文本帧 / SSE data 字段）
    pub fn to_json(&self) -> String { // 序列化为 JSON 字符串供下行发送
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string()) // 序列化失败时退化为空对象，避免 panic
    }
}
