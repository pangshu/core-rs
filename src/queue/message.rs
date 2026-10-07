//! 统一消息结构（文档 三·12）：id / topic / payload / headers / 重试计数。
//! 跨后端只保证 JSON 兼容类型不变；与 `realtime::RealtimeMessage` 保持一致的
//! 序列化约定（serde_json::Value 载荷）。

use std::collections::BTreeMap; // 引入有序映射，用于透传头键值对

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

/// 队列消息：`payload` 为 JSON 值载荷
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生 Debug/Clone 与 serde 序列化
pub struct Message { // 定义跨后端统一的消息结构
    /// 队列分配的消息 ID（redis = stream entry id；memory = 进程内自增序号）
    pub id: String, // 消息唯一 ID
    /// 路由主题
    pub topic: String, // 消息路由到的 topic
    /// 载荷（JSON 值）
    pub payload: serde_json::Value, // 业务载荷，JSON 值
    /// 透传头（跨后端保留；redis 存 JSON 字符串字段）
    #[serde(default)] // 反序列化缺失该字段时用默认空 map
    pub headers: BTreeMap<String, String>, // 透传头键值对
    /// 投递计数，从 1 起（Worker 重试会递增）
    pub attempts: u32, // 当前投递次数，从 1 起
}

impl Message {
    pub fn new(topic: impl Into<String>, payload: serde_json::Value) -> Self { // 构造一条新消息
        Self { // 以默认值填充字段
            id: crate::utils::new_id(), // 生成全局唯一消息 ID
            topic: topic.into(), // 目标 topic 转成 String
            payload, // 直接使用传入的载荷
            headers: BTreeMap::new(), // 头部初始化为空
            attempts: 1, // 首次投递计数为 1
        }
    }
}
