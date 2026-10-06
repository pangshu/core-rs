//! `[realtime]` 配置节：心跳间隔、最大连接数、跨实例转发开关（文档 三·15）。

use serde::{Deserialize, Serialize};

fn default_heartbeat_secs() -> u64 {
    30
}
fn default_max_connections() -> usize {
    10_000
}
fn default_send_buffer() -> usize {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeSettings {
    #[serde(default = "default_true_rt")]
    pub enabled: bool,
    /// WebSocket 心跳 ping 间隔（秒）
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    /// 单实例最大并发长连接数（超出拒绝升级）
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// 每连接下行缓冲条数：打满即**踢除慢消费者**（防止无界堆积 OOM）
    #[serde(default = "default_send_buffer")]
    pub send_buffer: usize,
    /// 跨实例广播转发：`off`（单机，默认）| `queue`（启用转发；v1 经
    /// Redis Pub/Sub 实现，要求 feature = "cache-redis" 且
    /// `[cache].backend = "redis"`，否则启动告警并保持单机广播）
    #[serde(default)]
    pub forward: String,
    /// 转发用 topic
    #[serde(default = "default_forward_topic")]
    pub forward_topic: String,
}

fn default_true_rt() -> bool {
    true
}
fn default_forward_topic() -> String {
    "__core_rs:realtime".to_string()
}

impl Default for RealtimeSettings {
    fn default() -> Self {
        Self {
            enabled: default_true_rt(),
            heartbeat_secs: default_heartbeat_secs(),
            max_connections: default_max_connections(),
            send_buffer: default_send_buffer(),
            forward: "off".to_string(),
            forward_topic: default_forward_topic(),
        }
    }
}
