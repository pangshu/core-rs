//! `[queue]` 配置节：backend + 并发度、重试、死信（文档 三·12）。
//!
//! backend = memory | redis | rabbitmq | kafka | nats，由 queue/mod.rs 的工厂选择。

use serde::{Deserialize, Serialize};

fn default_backend() -> String {
    "memory".to_string()
}
fn default_concurrency() -> usize {
    4
}
fn default_max_attempts() -> u32 {
    3
}
fn default_backoff_ms() -> u64 {
    1000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSettings {
    /// memory | redis | rabbitmq | kafka | nats
    #[serde(default = "default_backend")]
    pub backend: String,
    /// 单实例消费并发度（worker 任务数）
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// handler 失败重试次数（指数退避 1x/2x/4x…，上限 60s）；0 = 不重试
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// 重试基础退避（毫秒）
    #[serde(default = "default_backoff_ms")]
    pub retry_backoff_ms: u64,
    /// 死信 topic：重试耗尽的消息转发到这里（留空则只记 error 日志）
    #[serde(default)]
    pub dead_letter_topic: String,
    /// memory 后端参数
    #[serde(default)]
    pub memory: QueueMemorySettings,
    /// redis 后端参数（Redis Streams 消费组）
    #[serde(default)]
    pub redis: QueueRedisSettings,
    /// rabbitmq 后端参数
    #[serde(default)]
    pub rabbitmq: QueueRabbitmqSettings,
    /// kafka 后端参数
    #[serde(default)]
    pub kafka: QueueKafkaSettings,
    /// nats 后端参数（JetStream）
    #[serde(default)]
    pub nats: QueueNatsSettings,
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            concurrency: default_concurrency(),
            max_attempts: default_max_attempts(),
            retry_backoff_ms: default_backoff_ms(),
            dead_letter_topic: String::new(),
            memory: QueueMemorySettings::default(),
            redis: QueueRedisSettings::default(),
            rabbitmq: QueueRabbitmqSettings::default(),
            kafka: QueueKafkaSettings::default(),
            nats: QueueNatsSettings::default(),
        }
    }
}

/// `[queue.memory]`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueMemorySettings {
    /// 每个 topic 的 channel 缓冲条数；满了 publish 报错（不静默阻塞业务）
    #[serde(default = "default_buffer")]
    pub buffer: usize,
}

fn default_buffer() -> usize {
    1024
}

impl Default for QueueMemorySettings {
    fn default() -> Self {
        Self {
            buffer: default_buffer(),
        }
    }
}

/// `[queue.redis]`（Redis Streams 消费组语义）
#[derive(Clone, Serialize, Deserialize)]
pub struct QueueRedisSettings {
    #[serde(default)]
    pub url: String,
    /// 消费组名：同组多实例负载均衡分摊消息，异组各收一份
    #[serde(default = "default_group")]
    pub group: String,
    /// 消费者名（同组内区分实例）；留空自动生成
    #[serde(default)]
    pub consumer: String,
    /// stream 键前缀（多应用共享一个 Redis 时隔离键空间）
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// 单条消息最大投递次数：超过后不再自动接管，留在 pending 列表人工排查
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

impl Default for QueueRedisSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
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

impl QueueRedisSettings {
    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }
}

/// 手写 Debug：连接串脱敏（连接串里的密码常来自环境变量）
impl std::fmt::Debug for QueueRedisSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueRedisSettings")
            .field("url", &super::redact_url(&self.url))
            .field("group", &self.group)
            .field("consumer", &self.consumer)
            .field("key_prefix", &self.key_prefix)
            .field("max_attempts", &self.max_attempts)
            .field("block_secs", &self.block_secs)
            .field("claim_min_idle_secs", &self.claim_min_idle_secs)
            .field("batch", &self.batch)
            .finish()
    }
}

/// `[queue.rabbitmq]`
///
/// Default 手写与 serde 默认值对齐：toml 里缺整个 `[queue]` 节时走的是
/// `Default::default()`，derive 出来的 0/空串与 serde 默认（prefetch=16）不一致，
/// 会让 basic.qos 拿到与文档不同的值。
#[derive(Clone, Serialize, Deserialize)]
pub struct QueueRabbitmqSettings {
    /// amqp://user:pass@host:5672/%2f
    #[serde(default)]
    pub url: String,
    /// basic.qos 预取条数
    #[serde(default = "default_prefetch")]
    pub prefetch: u16,
}

fn default_prefetch() -> u16 {
    16
}

impl Default for QueueRabbitmqSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
            prefetch: default_prefetch(),
        }
    }
}

/// 手写 Debug：连接串脱敏
impl std::fmt::Debug for QueueRabbitmqSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueRabbitmqSettings")
            .field("url", &super::redact_url(&self.url))
            .field("prefetch", &self.prefetch)
            .finish()
    }
}

/// `[queue.kafka]`
#[derive(Clone, Serialize, Deserialize)]
pub struct QueueKafkaSettings {
    /// broker 列表，逗号分隔
    #[serde(default)]
    pub brokers: String,
    /// 消费组 id
    #[serde(default = "default_kafka_group")]
    pub group: String,
}

fn default_kafka_group() -> String {
    "core-rs".to_string()
}

impl Default for QueueKafkaSettings {
    fn default() -> Self {
        Self {
            brokers: String::new(),
            group: default_kafka_group(),
        }
    }
}

impl std::fmt::Debug for QueueKafkaSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueKafkaSettings")
            .field("brokers", &self.brokers)
            .field("group", &self.group)
            .finish()
    }
}

/// `[queue.nats]`（JetStream）
#[derive(Clone, Serialize, Deserialize)]
pub struct QueueNatsSettings {
    /// nats://127.0.0.1:4222
    #[serde(default)]
    pub url: String,
    /// JetStream stream 名
    #[serde(default = "default_nats_stream")]
    pub stream: String,
    /// durable consumer 名
    #[serde(default = "default_nats_durable")]
    pub durable: String,
    /// 单条消息最大投递次数：超过后服务端不再重投（防毒消息无限循环）
    #[serde(default = "default_nats_max_deliver")]
    pub max_deliver: i64,
}

fn default_nats_stream() -> String {
    "core-rs".to_string()
}
fn default_nats_durable() -> String {
    "core-rs".to_string()
}
fn default_nats_max_deliver() -> i64 {
    5
}

impl Default for QueueNatsSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
            stream: default_nats_stream(),
            durable: default_nats_durable(),
            max_deliver: default_nats_max_deliver(),
        }
    }
}

impl std::fmt::Debug for QueueNatsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueNatsSettings")
            .field("url", &super::redact_url(&self.url))
            .field("stream", &self.stream)
            .field("durable", &self.durable)
            .field("max_deliver", &self.max_deliver)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// derive/serde 默认值一致性（P1-43）：节缺失（走 Default::default()）与
    /// 显式空配置（走 serde default）必须给出相同值
    #[test]
    fn derived_default_matches_serde_default() {
        let from_json: QueueRabbitmqSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(QueueRabbitmqSettings::default().prefetch, from_json.prefetch);

        let from_json: QueueKafkaSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(QueueKafkaSettings::default().group, from_json.group);

        let from_json: QueueNatsSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(QueueNatsSettings::default().stream, from_json.stream);
        assert_eq!(QueueNatsSettings::default().durable, from_json.durable);
    }
}
