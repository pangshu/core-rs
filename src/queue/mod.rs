//! 消息队列抽象（feature = "queue"）：统一 [`Queue`] 契约 + memory / redis 双后端，
//! 由 `[queue]` 配置段选择（auto = `[redis].url` 非空走 redis，否则 memory）。
//!
//! 契约对标 go-admin-core storage/Queue：
//! - `publish` 时 topic 无订阅者返回 [`QueueError::NoHandler`]（不静默丢弃）；
//! - `subscribe` 必须在 `start` 之前（`Application` 装配时先注册后启动，业务无感）；
//! - `close` 幂等：memory 排空在途消息后退出；redis 不排空，未 ACK 消息留在
//!   pending 列表由其他实例接管（at-least-once，消费方须幂等）。
//!
//! handler 通过 `Application::builder().queue_task(topic, |msg| async { ... })` 注册，
//! 业务侧发布用 `state.queue.publish("topic", serde_json::json!({...}))`。
//! 其他 MQ（rabbitmq / kafka 等）实现 [`Queue`] trait 即可接入同一契约。

pub mod memory;
pub mod redis_stream;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 队列消息：`values` 为 JSON 对象载荷（跨后端只保证 JSON 兼容类型不变）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// 队列分配的消息 ID（redis = stream entry id；memory = 进程内自增序号）
    pub id: String,
    /// 路由主题
    pub topic: String,
    /// 载荷（JSON 对象）
    pub values: serde_json::Value,
    /// 投递计数，从 1 起（重试/接管会递增）
    pub attempts: u32,
}

/// 消费回调
pub type Handler =
    Arc<dyn Fn(Message) -> Pin<Box<dyn Future<Output = Result<(), QueueError>> + Send>> + Send + Sync>;

/// 队列操作错误
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("queue backend not configured: {0}")]
    Config(String),
    #[error("no subscriber for topic `{0}`")]
    NoHandler(String),
    #[error("buffer full for topic `{0}` (publish would block, raise [queue.memory].buffer)")]
    Full(String),
    #[error("topic `{0}` already subscribed")]
    AlreadySubscribed(String),
    #[error("queue already started")]
    AlreadyStarted,
    #[error("queue closed")]
    Closed,
}

/// 队列契约。`publish` / `subscribe` / `start` / `close` 语义见模块文档。
#[async_trait::async_trait]
pub trait Queue: Send + Sync {
    /// 发布消息到 topic（无订阅者报 [`QueueError::NoHandler`]），返回消息 ID
    async fn publish(&self, topic: &str, values: serde_json::Value) -> Result<String, QueueError>;
    /// 订阅 topic（必须在 [`Queue::start`] 之前；重复订阅报错）
    async fn subscribe(&self, topic: &str, handler: Handler) -> Result<(), QueueError>;
    /// 启动消费（后台任务执行，业务无感）
    async fn start(self: Arc<Self>) -> Result<(), QueueError>;
    /// 优雅关闭（幂等）
    async fn close(&self) -> Result<(), QueueError>;
}

/// 共享队列句柄（存在 [`crate::state::AppState`] 与 builder 之间）
pub type QueueHandle = Arc<dyn Queue>;

/// 按 `[queue]` + `[redis]` 配置构建队列实例（Application 装配时自动调用）。
/// redis 后端建连失败（坏地址）在装配期报错。
pub async fn build(
    cfg: &crate::config::QueueConfig,
    redis_cfg: &crate::config::RedisConfig,
) -> Result<Option<QueueHandle>, QueueError> {
    match cfg.backend.as_str() {
        "redis" => {
            if redis_cfg.url.is_empty() {
                return Err(QueueError::Config(
                    "queue.type = redis but redis.url is empty".to_string(),
                ));
            }
            Ok(Some(Arc::new(
                redis_stream::RedisQueue::new(&redis_cfg.url, &cfg.redis).await?,
            )))
        }
        "memory" => Ok(Some(Arc::new(memory::MemoryQueue::new(&cfg.memory)))),
        "auto" => {
            if !redis_cfg.url.is_empty() {
                Ok(Some(Arc::new(
                    redis_stream::RedisQueue::new(&redis_cfg.url, &cfg.redis).await?,
                )))
            } else {
                Ok(Some(Arc::new(memory::MemoryQueue::new(&cfg.memory))))
            }
        }
        other => Err(QueueError::Config(format!(
            "unknown queue.type: {other} (expected auto / redis / memory)"
        ))),
    }
}
