//! `[realtime]` 配置节：心跳间隔、最大连接数、跨实例转发开关（文档 三·15）。

use serde::{Deserialize, Serialize}; // 引入 serde 反序列化/序列化派生宏

fn default_heartbeat_secs() -> u64 { // 心跳间隔的默认值函数
    30 // 默认 30 秒心跳
}
fn default_max_connections() -> usize { // 最大连接数的默认值函数
    10_000 // 默认最多 1 万连接
}
fn default_send_buffer() -> usize { // 下行缓冲的默认值函数
    64 // 默认每连接缓冲 64 条
}

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 能力
pub struct RealtimeSettings { // 实时通信配置结构
    #[serde(default = "default_true_rt")] // 缺失时默认启用
    pub enabled: bool, // 是否启用实时通信
    /// WebSocket 心跳 ping 间隔（秒）
    #[serde(default = "default_heartbeat_secs")] // 缺失时用默认间隔
    pub heartbeat_secs: u64, // 心跳 ping 间隔秒数
    /// 单实例最大并发长连接数（超出拒绝升级）
    #[serde(default = "default_max_connections")] // 缺失时用默认上限
    pub max_connections: usize, // 单实例最大并发长连接数
    /// 每连接下行缓冲条数：打满即**踢除慢消费者**（防止无界堆积 OOM）
    #[serde(default = "default_send_buffer")] // 缺失时用默认缓冲
    pub send_buffer: usize, // 每连接下行缓冲条数
    /// 跨实例广播转发：`off`（单机，默认）| `queue`（启用转发；v1 经
    /// Redis Pub/Sub 实现，要求 feature = "cache-redis" 且
    /// `[cache].backend = "redis"`，否则启动告警并保持单机广播）
    #[serde(default)] // 缺失时用默认值
    pub forward: String, // 跨实例转发模式
    /// 转发用 topic
    #[serde(default = "default_forward_topic")] // 缺失时用默认 topic
    pub forward_topic: String, // 跨实例转发使用的 topic
}

fn default_true_rt() -> bool { // 启用开关的默认值函数
    true // 默认启用实时通信
}
fn default_forward_topic() -> String { // 转发 topic 的默认值函数
    "__core_rs:realtime".to_string() // 默认转发 topic 名
}

impl Default for RealtimeSettings { // 为实时通信配置实现 Default
    fn default() -> Self { // 返回默认配置
        Self { // 构造默认配置
            enabled: default_true_rt(), // 默认启用
            heartbeat_secs: default_heartbeat_secs(), // 默认心跳间隔
            max_connections: default_max_connections(), // 默认最大连接数
            send_buffer: default_send_buffer(), // 默认下行缓冲
            forward: "off".to_string(), // 默认单机广播
            forward_topic: default_forward_topic(), // 默认转发 topic
        }
    }
}
