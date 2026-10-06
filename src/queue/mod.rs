//! 消息队列抽象（文档 三·12）：统一 [`Queue`] 契约 + 可插拔后端，由 `[queue]` 配置选择。
//!
//! - **任务分发**（一条消息一个 worker 处理，如转码、发信、上传回调）与
//!   **事件广播**（异消费组各收一份）共用同一套抽象；
//! - `publish` 到未注册的 topic 返回 [`QueueError::NoHandler`]（不静默丢弃）；
//! - 消费端统一由 [`Worker`](worker::Worker) 驱动：并发消费、失败重试（指数退避）、
//!   死信转发、随 App 优雅停机；
//! - 与 `task/` 的分工：queue 管异步消息投递，task 管周期调度（cron）。

pub mod message;
pub mod worker;

#[cfg(feature = "queue-memory")]
pub mod memory;
#[cfg(feature = "queue-redis")]
pub mod redis;
#[cfg(feature = "queue-rabbitmq")]
pub mod rabbitmq;
#[cfg(feature = "queue-kafka")]
pub mod kafka;
#[cfg(feature = "queue-nats")]
pub mod nats;

use std::collections::BTreeMap;

pub use message::Message;

use crate::config::sections::QueueSettings;

/// 队列操作错误
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("queue backend not configured: {0}")]
    Config(String),
    #[error("no handler registered for topic `{0}`")]
    NoHandler(String),
    #[error("buffer full for topic `{0}` (publish would block, raise [queue.memory].buffer)")]
    Full(String),
    #[error("topic `{0}` already registered")]
    AlreadyRegistered(String),
    #[error("queue already closed")]
    Closed,
    #[error("queue backend error: {0}")]
    Backend(String),
    #[error("queue serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// 已投递、待确认的消息（`ack_token` 为后端私有确认凭据，业务侧不感知）
#[derive(Debug, Clone)]
pub struct Delivery {
    pub message: Message,
    pub ack_token: String,
}

/// 队列契约（拉取式，天然适配 memory / redis stream / rabbitmq / kafka / nats）。
#[async_trait::async_trait]
pub trait Queue: Send + Sync {
    /// 后端名（日志用）
    fn name(&self) -> &'static str;

    /// 注册 topic（必须在消费启动前；redis = 建消费组，rabbitmq = 声明队列）。
    /// 重复注册报 [`QueueError::AlreadyRegistered`]。
    async fn register(&self, topic: &str) -> Result<(), QueueError>;

    /// 发布消息到 topic（未注册报 [`QueueError::NoHandler`]），返回消息 ID
    async fn publish(
        &self,
        topic: &str,
        payload: serde_json::Value,
        headers: BTreeMap<String, String>,
    ) -> Result<String, QueueError>;

    /// 拉取一批消息（内部阻塞至拿到消息或 block 超时；空 Vec = 本轮无消息）。
    /// 只包含已注册的 topic。
    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError>;

    /// 确认消费成功
    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError>;

    /// 确认消费失败（后端自行决定：redis 留 pending 待接管，rabbitmq reject，
    /// memory 记日志丢弃——重试语义统一由 Worker 承担）
    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError>;

    /// 优雅关闭（幂等）：memory 排空在途消息后退出；redis/rabbitmq 不排空，
    /// 未 ACK 消息由其他实例接管（at-least-once，消费方须幂等）
    async fn close(&self) -> Result<(), QueueError>;

    /// 存活探测（/ready 用）
    async fn ping(&self) -> Result<(), QueueError> {
        Ok(())
    }
}

/// 共享队列句柄（存于 CoreState）
pub type QueueHandle = std::sync::Arc<dyn Queue>;

/// 按 `[queue]` 配置构建队列实例（App 装配时自动调用；手动装配亦可用）。
/// 后端建连失败（坏地址）在装配期报错。
pub async fn build(settings: &QueueSettings) -> Result<QueueHandle, QueueError> {
    match settings.backend.as_str() {
        "memory" => {
            #[cfg(feature = "queue-memory")]
            {
                Ok(std::sync::Arc::new(memory::MemoryQueue::new(settings)))
            }
            #[cfg(not(feature = "queue-memory"))]
            {
                Err(QueueError::feature_disabled("memory"))
            }
        }
        "redis" => {
            #[cfg(feature = "queue-redis")]
            {
                if !settings.redis.enabled() {
                    return Err(QueueError::Config(
                        "queue.backend = redis but queue.redis.url is empty".to_string(),
                    ));
                }
                Ok(std::sync::Arc::new(
                    redis::RedisQueue::new(&settings.redis).await?,
                ))
            }
            #[cfg(not(feature = "queue-redis"))]
            {
                Err(QueueError::Config(
                    "queue.backend = redis requires feature queue-redis".to_string(),
                ))
            }
        }
        "rabbitmq" => {
            #[cfg(feature = "queue-rabbitmq")]
            {
                Ok(std::sync::Arc::new(
                    rabbitmq::RabbitmqQueue::connect(&settings.rabbitmq).await?,
                ))
            }
            #[cfg(not(feature = "queue-rabbitmq"))]
            {
                Err(QueueError::Config(
                    "queue.backend = rabbitmq requires feature queue-rabbitmq".to_string(),
                ))
            }
        }
        "kafka" => {
            #[cfg(feature = "queue-kafka")]
            {
                Ok(std::sync::Arc::new(kafka::KafkaQueue::connect(&settings.kafka).await?))
            }
            #[cfg(not(feature = "queue-kafka"))]
            {
                Err(QueueError::Config(
                    "queue.backend = kafka requires feature queue-kafka".to_string(),
                ))
            }
        }
        "nats" => {
            #[cfg(feature = "queue-nats")]
            {
                Ok(std::sync::Arc::new(nats::NatsQueue::connect(&settings.nats).await?))
            }
            #[cfg(not(feature = "queue-nats"))]
            {
                Err(QueueError::Config(
                    "queue.backend = nats requires feature queue-nats".to_string(),
                ))
            }
        }
        other => Err(QueueError::Config(format!(
            "unknown queue.backend: {other} (expected memory / redis / rabbitmq / kafka / nats)"
        ))),
    }
}

impl QueueError {
    #[allow(dead_code)] // 仅在可选后端 feature 关闭的编译组合中使用
    pub(crate) fn feature_disabled(backend: &str) -> Self {
        QueueError::Config(format!(
            "queue backend `{backend}` requires its feature to be enabled"
        ))
    }
}
