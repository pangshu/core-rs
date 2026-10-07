//! `[queue]` 配置节：backend + 并发度、重试、死信（文档 三·12）。
//!
//! backend = memory | redis | rabbitmq | kafka | nats，由 queue/mod.rs 的工厂选择。

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

fn default_backend() -> String { // 默认队列后端取值函数
    "memory".to_string() // 默认 memory 后端
}
fn default_concurrency() -> usize { // 默认并发度取值函数
    4 // 默认 4 个 worker
}
fn default_max_attempts() -> u32 { // 默认重试次数取值函数
    3 // 默认重试 3 次
}
fn default_backoff_ms() -> u64 { // 默认退避取值函数
    1000 // 默认 1000 毫秒
}

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct QueueSettings { // 定义 `[queue]` 配置结构体
    /// memory | redis | rabbitmq | kafka | nats
    #[serde(default = "default_backend")] // 缺省为 memory
    pub backend: String, // 队列后端选择
    /// 单实例消费并发度（worker 任务数）
    #[serde(default = "default_concurrency")] // 缺省为 4
    pub concurrency: usize, // 消费并发度
    /// handler 失败重试次数（指数退避 1x/2x/4x…，上限 60s）；0 = 不重试
    #[serde(default = "default_max_attempts")] // 缺省为 3
    pub max_attempts: u32, // 失败重试次数
    /// 重试基础退避（毫秒）
    #[serde(default = "default_backoff_ms")] // 缺省为 1000ms
    pub retry_backoff_ms: u64, // 重试基础退避（毫秒）
    /// 死信 topic：重试耗尽的消息转发到这里（留空则只记 error 日志）
    #[serde(default)] // 缺省为空串
    pub dead_letter_topic: String, // 死信 topic
    /// memory 后端参数
    #[serde(default)] // 缺省用 memory 默认值
    pub memory: QueueMemorySettings, // memory 后端参数
    /// redis 后端参数（Redis Streams 消费组）
    #[serde(default)] // 缺省用 redis 默认值
    pub redis: QueueRedisSettings, // redis 后端参数
    /// rabbitmq 后端参数
    #[serde(default)] // 缺省用 rabbitmq 默认值
    pub rabbitmq: QueueRabbitmqSettings, // rabbitmq 后端参数
    /// kafka 后端参数
    #[serde(default)] // 缺省用 kafka 默认值
    pub kafka: QueueKafkaSettings, // kafka 后端参数
    /// nats 后端参数（JetStream）
    #[serde(default)] // 缺省用 nats 默认值
    pub nats: QueueNatsSettings, // nats 后端参数
}

impl Default for QueueSettings { // 为 QueueSettings 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            backend: default_backend(), // 默认 memory
            concurrency: default_concurrency(), // 默认并发 4
            max_attempts: default_max_attempts(), // 默认重试 3 次
            retry_backoff_ms: default_backoff_ms(), // 默认退避 1000ms
            dead_letter_topic: String::new(), // 默认无死信 topic
            memory: QueueMemorySettings::default(), // memory 默认参数
            redis: QueueRedisSettings::default(), // redis 默认参数
            rabbitmq: QueueRabbitmqSettings::default(), // rabbitmq 默认参数
            kafka: QueueKafkaSettings::default(), // kafka 默认参数
            nats: QueueNatsSettings::default(), // nats 默认参数
        }
    }
}

/// `[queue.memory]`
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct QueueMemorySettings { // 定义 `[queue.memory]` 配置
    /// 每个 topic 的 channel 缓冲条数；满了 publish 报错（不静默阻塞业务）
    #[serde(default = "default_buffer")] // 缺省为 1024
    pub buffer: usize, // 每 topic 缓冲条数
}

fn default_buffer() -> usize { // 缓冲默认值函数
    1024 // 默认 1024 条
}

impl Default for QueueMemorySettings { // 为 memory 配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            buffer: default_buffer(), // 默认 1024
        }
    }
}

/// `[queue.redis]`（Redis Streams 消费组语义）
#[derive(Clone, Serialize, Deserialize)] // 派生克隆与 serde（Debug 手写）
pub struct QueueRedisSettings { // 定义 `[queue.redis]` 配置
    #[serde(default)] // 缺省为空串（未启用）
    pub url: String, // Redis 连接串
    /// 消费组名：同组多实例负载均衡分摊消息，异组各收一份
    #[serde(default = "default_group")] // 缺省为 core-rs
    pub group: String, // 消费组名
    /// 消费者名（同组内区分实例）；留空自动生成
    #[serde(default)] // 缺省为空串（自动生成）
    pub consumer: String, // 消费者名
    /// stream 键前缀（多应用共享一个 Redis 时隔离键空间）
    #[serde(default = "default_key_prefix")] // 缺省为 core-rs:queue:
    pub key_prefix: String, // stream 键前缀
    /// 单条消息最大投递次数：超过后不再自动接管，留在 pending 列表人工排查
    #[serde(default = "default_redis_max_attempts")] // 缺省为 3
    pub max_attempts: u64, // 单条消息最大投递次数
    /// XREADGROUP 的 BLOCK 时长（秒），也是无消息时的轮询间隔
    #[serde(default = "default_block_secs")] // 缺省为 1
    pub block_secs: u64, // XREADGROUP BLOCK 时长（秒）
    /// pending 消息接管阈值（秒）：某消费者宕机后，其未 ACK 消息闲置超过该时长
    /// 会被本实例 XCLAIM 接手重投
    #[serde(default = "default_claim_min_idle_secs")] // 缺省为 30
    pub claim_min_idle_secs: u64, // pending 接管闲置阈值（秒）
    /// 每轮 XREADGROUP / XCLAIM 的批量条数
    #[serde(default = "default_batch")] // 缺省为 16
    pub batch: usize, // 每轮批量条数
}

fn default_group() -> String { // 消费组名默认值函数
    "core-rs".to_string() // 默认 core-rs
}
fn default_key_prefix() -> String { // 键前缀默认值函数
    "core-rs:queue:".to_string() // 默认 core-rs:queue:
}
fn default_redis_max_attempts() -> u64 { // redis 最大投递默认值函数
    3 // 默认 3 次
}
fn default_block_secs() -> u64 { // BLOCK 时长默认值函数
    1 // 默认 1 秒
}
fn default_claim_min_idle_secs() -> u64 { // 接管阈值默认值函数
    30 // 默认 30 秒
}
fn default_batch() -> usize { // 批量条数默认值函数
    16 // 默认 16 条
}

impl Default for QueueRedisSettings { // 为 redis 配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            url: String::new(), // 默认无连接串
            group: default_group(), // 默认 core-rs
            consumer: String::new(), // 默认自动生成消费者名
            key_prefix: default_key_prefix(), // 默认 core-rs:queue:
            max_attempts: default_redis_max_attempts(), // 默认 3 次
            block_secs: default_block_secs(), // 默认 1 秒
            claim_min_idle_secs: default_claim_min_idle_secs(), // 默认 30 秒
            batch: default_batch(), // 默认 16 条
        }
    }
}

impl QueueRedisSettings { // 为 redis 配置实现方法
    pub fn enabled(&self) -> bool { // 判断 redis 后端是否已配置
        !self.url.is_empty() // url 非空即启用
    }
}

/// 手写 Debug：连接串脱敏（连接串里的密码常来自环境变量）
impl std::fmt::Debug for QueueRedisSettings { // 手写 Debug，连接串脱敏
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("QueueRedisSettings") // 开始构造调试输出
            .field("url", &super::redact_url(&self.url)) // url 脱敏输出
            .field("group", &self.group) // 输出消费组名
            .field("consumer", &self.consumer) // 输出消费者名
            .field("key_prefix", &self.key_prefix) // 输出键前缀
            .field("max_attempts", &self.max_attempts) // 输出最大投递次数
            .field("block_secs", &self.block_secs) // 输出 BLOCK 时长
            .field("claim_min_idle_secs", &self.claim_min_idle_secs) // 输出接管阈值
            .field("batch", &self.batch) // 输出批量条数
            .finish() // 结束并生成调试输出
    }
}

/// `[queue.rabbitmq]`
///
/// Default 手写与 serde 默认值对齐：toml 里缺整个 `[queue]` 节时走的是
/// `Default::default()`，derive 出来的 0/空串与 serde 默认（prefetch=16）不一致，
/// 会让 basic.qos 拿到与文档不同的值。
#[derive(Clone, Serialize, Deserialize)] // 派生克隆与 serde（Default/Debug 手写）
pub struct QueueRabbitmqSettings { // 定义 `[queue.rabbitmq]` 配置
    /// amqp://user:pass@host:5672/%2f
    #[serde(default)] // 缺省为空串（未启用）
    pub url: String, // AMQP 连接串
    /// basic.qos 预取条数
    #[serde(default = "default_prefetch")] // 缺省为 16
    pub prefetch: u16, // basic.qos 预取条数
}

fn default_prefetch() -> u16 { // 预取条数默认值函数
    16 // 默认 16 条
}

impl Default for QueueRabbitmqSettings { // 手写默认值，与 serde 默认对齐
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            url: String::new(), // 默认无连接串
            prefetch: default_prefetch(), // 默认 16
        }
    }
}

/// 手写 Debug：连接串脱敏
impl std::fmt::Debug for QueueRabbitmqSettings { // 手写 Debug，连接串脱敏
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("QueueRabbitmqSettings") // 开始构造调试输出
            .field("url", &super::redact_url(&self.url)) // url 脱敏输出
            .field("prefetch", &self.prefetch) // 输出预取条数
            .finish() // 结束并生成调试输出
    }
}

/// `[queue.kafka]`
#[derive(Clone, Serialize, Deserialize)] // 派生克隆与 serde（Default/Debug 手写）
pub struct QueueKafkaSettings { // 定义 `[queue.kafka]` 配置
    /// broker 列表，逗号分隔
    #[serde(default)] // 缺省为空串（未启用）
    pub brokers: String, // broker 列表
    /// 消费组 id
    #[serde(default = "default_kafka_group")] // 缺省为 core-rs
    pub group: String, // 消费组 id
}

fn default_kafka_group() -> String { // kafka 消费组默认值函数
    "core-rs".to_string() // 默认 core-rs
}

impl Default for QueueKafkaSettings { // 手写默认值，与 serde 默认对齐
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            brokers: String::new(), // 默认无 broker
            group: default_kafka_group(), // 默认 core-rs
        }
    }
}

impl std::fmt::Debug for QueueKafkaSettings { // 手写 Debug 实现
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("QueueKafkaSettings") // 开始构造调试输出
            .field("brokers", &self.brokers) // 输出 broker 列表
            .field("group", &self.group) // 输出消费组 id
            .finish() // 结束并生成调试输出
    }
}

/// `[queue.nats]`（JetStream）
#[derive(Clone, Serialize, Deserialize)] // 派生克隆与 serde（Default/Debug 手写）
pub struct QueueNatsSettings { // 定义 `[queue.nats]` 配置
    /// nats://127.0.0.1:4222
    #[serde(default)] // 缺省为空串（未启用）
    pub url: String, // NATS 连接串
    /// JetStream stream 名
    #[serde(default = "default_nats_stream")] // 缺省为 core-rs
    pub stream: String, // JetStream stream 名
    /// durable consumer 名
    #[serde(default = "default_nats_durable")] // 缺省为 core-rs
    pub durable: String, // durable consumer 名
    /// 单条消息最大投递次数：超过后服务端不再重投（防毒消息无限循环）
    #[serde(default = "default_nats_max_deliver")] // 缺省为 5
    pub max_deliver: i64, // 单条消息最大投递次数
}

fn default_nats_stream() -> String { // stream 名默认值函数
    "core-rs".to_string() // 默认 core-rs
}
fn default_nats_durable() -> String { // durable 名默认值函数
    "core-rs".to_string() // 默认 core-rs
}
fn default_nats_max_deliver() -> i64 { // 最大投递默认值函数
    5 // 默认 5 次
}

impl Default for QueueNatsSettings { // 手写默认值，与 serde 默认对齐
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            url: String::new(), // 默认无连接串
            stream: default_nats_stream(), // 默认 core-rs
            durable: default_nats_durable(), // 默认 core-rs
            max_deliver: default_nats_max_deliver(), // 默认 5 次
        }
    }
}

impl std::fmt::Debug for QueueNatsSettings { // 手写 Debug，连接串脱敏
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("QueueNatsSettings") // 开始构造调试输出
            .field("url", &super::redact_url(&self.url)) // url 脱敏输出
            .field("stream", &self.stream) // 输出 stream 名
            .field("durable", &self.durable) // 输出 durable 名
            .field("max_deliver", &self.max_deliver) // 输出最大投递次数
            .finish() // 结束并生成调试输出
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
