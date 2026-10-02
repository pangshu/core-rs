use serde::{Deserialize, Serialize};

fn default_queue_type() -> String {
    "auto".to_string()
}

/// `[queue]` 配置段（feature = "queue"）：消息队列后端选择。
///
/// - `memory`：进程内队列（tokio mpsc，失败重试 + 退避，重启即丢）
/// - `redis`：Redis Streams 消费组（at-least-once，多实例分摊消费、宕机消息被接管）
/// - `auto`（默认）：`[redis].url` 非空走 redis，否则走 memory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    /// auto | memory | redis
    #[serde(rename = "type", default = "default_queue_type")]
    pub backend: String,
    /// memory 后端参数
    #[serde(default)]
    pub memory: QueueMemoryConfig,
    /// redis 后端参数（Redis Streams）
    #[serde(default)]
    pub redis: QueueRedisConfig,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            backend: default_queue_type(),
            memory: QueueMemoryConfig::default(),
            redis: QueueRedisConfig::default(),
        }
    }
}

/// `[queue.memory]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueMemoryConfig {
    /// 每个 topic 的 channel 缓冲条数；满了之后 publish 报错（不静默阻塞业务）
    #[serde(default = "default_buffer")]
    pub buffer: usize,
    /// 消费失败重试次数（退避 1s/2s/3s…）；0 = 不重试
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

impl Default for QueueMemoryConfig {
    fn default() -> Self {
        Self {
            buffer: default_buffer(),
            max_attempts: default_max_attempts(),
        }
    }
}

fn default_buffer() -> usize {
    1024
}
fn default_max_attempts() -> u32 {
    3
}

/// `[queue.redis]` 配置段（Redis Streams 消费组语义，连接复用 `[redis].url`）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueRedisConfig {
    /// 消费组名：同组多实例负载均衡分摊消息，异组各收一份
    #[serde(default = "default_group")]
    pub group: String,
    /// 消费者名（同组内区分实例）；留空自动生成 `hostname-pid-uuid前缀`
    #[serde(default)]
    pub consumer: String,
    /// stream 键前缀（多应用共享一个 Redis 时隔离键空间）
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// 单条消息最大投递次数：超过后不再自动接管/重试，留在 pending 列表人工排查
    #[serde(default = "default_redis_max_attempts")]
    pub max_attempts: u64,
    /// XREADGROUP 的 BLOCK 时长（秒），也是无消息时的轮询间隔
    #[serde(default = "default_block_secs")]
    pub block_secs: u64,
    /// pending 消息接管阈值（秒）：某消费者宕机后，其未 ACK 消息闲置超过该时长
    /// 会被本实例 XCLAIM 接手重投
    #[serde(default = "default_claim_min_idle_secs")]
    pub claim_min_idle_secs: u64,
    /// 每轮 XREADGROUP / XCLAIM 的批量条数
    #[serde(default = "default_batch")]
    pub batch: usize,
}

impl Default for QueueRedisConfig {
    fn default() -> Self {
        Self {
            group: default_group(),
            consumer: String::new(),
            key_prefix: default_key_prefix(),
            max_attempts: default_redis_max_attempts(),
            block_secs: default_block_secs(),
            claim_min_idle_secs: default_claim_min_idle_secs(),
            batch: default_batch(),
        }
    }
}

fn default_group() -> String {
    "core-rs".to_string()
}
fn default_key_prefix() -> String {
    "core-rs:queue:".to_string()
}
fn default_redis_max_attempts() -> u64 {
    3
}
fn default_block_secs() -> u64 {
    1
}
fn default_claim_min_idle_secs() -> u64 {
    30
}
fn default_batch() -> usize {
    16
}
