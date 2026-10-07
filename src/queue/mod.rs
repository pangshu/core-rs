//! 消息队列抽象（文档 三·12）：统一 [`Queue`] 契约 + 可插拔后端，由 `[queue]` 配置选择。
//!
//! - **任务分发**（一条消息一个 worker 处理，如转码、发信、上传回调）与
//!   **事件广播**（异消费组各收一份）共用同一套抽象；
//! - `publish` 到未注册的 topic 返回 [`QueueError::NoHandler`]（不静默丢弃）；
//! - 消费端统一由 [`Worker`](worker::Worker) 驱动：并发消费、失败重试（指数退避）、
//!   死信转发、随 App 优雅停机；
//! - 与 `task/` 的分工：queue 管异步消息投递，task 管周期调度（cron）。

pub mod message; // 声明消息结构子模块（Message）
pub mod worker; // 声明消费者 Worker 子模块（并发消费/重试/死信）

#[cfg(feature = "queue-memory")] // 开启内存队列 feature 时编译下面模块
pub mod memory; // 进程内内存队列后端
#[cfg(feature = "queue-redis")] // 开启 redis 队列 feature 时编译下面模块
pub mod redis; // Redis Stream 队列后端
#[cfg(feature = "queue-rabbitmq")] // 开启 rabbitmq 队列 feature 时编译下面模块
pub mod rabbitmq; // RabbitMQ 队列后端
#[cfg(feature = "queue-kafka")] // 开启 kafka 队列 feature 时编译下面模块
pub mod kafka; // Kafka 队列后端
#[cfg(feature = "queue-nats")] // 开启 nats 队列 feature 时编译下面模块
pub mod nats; // NATS 队列后端

use std::collections::BTreeMap; // 引入有序映射，用于消息 headers 的透传键值

pub use message::Message; // 对外重导出消息类型，使用方无需关心子模块路径

use crate::config::sections::QueueSettings; // 引入 `[queue]` 配置段类型，供 build 读取

/// 队列操作错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 并让 thiserror 生成 Error 实现
pub enum QueueError { // 定义队列统一错误枚举
    #[error("queue backend not configured: {0}")] // 配置缺失/非法时的错误消息模板
    Config(String), // 配置类错误（携带说明文本）
    #[error("no handler registered for topic `{0}`")] // 发布到未注册 topic 的错误模板
    NoHandler(String), // 目标 topic 未注册错误（携带 topic 名）
    #[error("buffer full for topic `{0}` (publish would block, raise [queue.memory].buffer)")] // 内存队列缓冲已满的错误模板
    Full(String), // 缓冲区已满错误（携带 topic 名）
    #[error("topic `{0}` already registered")] // 重复注册 topic 的错误模板
    AlreadyRegistered(String), // topic 重复注册错误（携带 topic 名）
    #[error("queue already closed")] // 队列已关闭时的错误模板
    Closed, // 队列已关闭错误
    #[error("queue backend error: {0}")] // 后端自身报错时的错误模板
    Backend(String), // 后端实现层错误（携带说明文本）
    #[error("queue serialization error: {0}")] // 载荷序列化失败时的错误模板
    Serde(#[from] serde_json::Error), // 由 serde_json 错误自动转换而来
}

/// 已投递、待确认的消息（`ack_token` 为后端私有确认凭据，业务侧不感知）
#[derive(Debug, Clone)] // 派生 Debug 与 Clone，便于日志打印与跨任务克隆
pub struct Delivery { // 定义一次投递结果结构
    pub message: Message, // 实际消息内容
    pub ack_token: String, // 后端私有确认凭据（业务不感知）
}

/// 队列契约（拉取式，天然适配 memory / redis stream / rabbitmq / kafka / nats）。
#[async_trait::async_trait] // 用 async_trait 宏把 async trait 方法降级为可对象安全的形态
pub trait Queue: Send + Sync { // 定义队列后端统一接口，要求可跨线程共享
    /// 后端名（日志用）
    fn name(&self) -> &'static str; // 返回后端名称，用于日志与诊断

    /// 注册 topic（必须在消费启动前；redis = 建消费组，rabbitmq = 声明队列）。
    /// 重复注册报 [`QueueError::AlreadyRegistered`]。
    async fn register(&self, topic: &str) -> Result<(), QueueError>; // 注册一个 topic，须在消费启动前调用

    /// 发布消息到 topic（未注册报 [`QueueError::NoHandler`]），返回消息 ID
    async fn publish( // 发布消息到指定 topic
        &self, // 自身引用
        topic: &str, // 目标 topic 名
        payload: serde_json::Value, // JSON 载荷
        headers: BTreeMap<String, String>, // 透传头键值对
    ) -> Result<String, QueueError>; // 成功返回分配的消息 ID

    /// 拉取一批消息（内部阻塞至拿到消息或 block 超时；空 Vec = 本轮无消息）。
    /// 只包含已注册的 topic。
    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError>; // 拉取最多 max 条待确认消息

    /// 确认消费成功
    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError>; // 确认某条消息消费成功

    /// 确认消费失败（后端自行决定：redis 留 pending 待接管，rabbitmq reject，
    /// memory 记日志丢弃——重试语义统一由 Worker 承担）
    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError>; // 确认消费失败，交由后端处置

    /// 优雅关闭（幂等）：memory 排空在途消息后退出；redis/rabbitmq 不排空，
    /// 未 ACK 消息由其他实例接管（at-least-once，消费方须幂等）
    async fn close(&self) -> Result<(), QueueError>; // 幂等关闭队列，触发优雅停机

    /// 存活探测（/ready 用）
    async fn ping(&self) -> Result<(), QueueError> { // 默认存活探测实现（后端可覆盖）
        Ok(()) // 默认视为存活，返回成功
    }
}

/// 共享队列句柄（存于 CoreState）
pub type QueueHandle = std::sync::Arc<dyn Queue>; // 用 Arc 包裹 trait 对象，便于跨线程共享

/// 按 `[queue]` 配置构建队列实例（App 装配时自动调用；手动装配亦可用）。
/// 后端建连失败（坏地址）在装配期报错。
pub async fn build(settings: &QueueSettings) -> Result<QueueHandle, QueueError> { // 依据配置构建并返回队列句柄
    match settings.backend.as_str() { // 按配置的 backend 字符串分派
        "memory" => { // 内存后端分支
            #[cfg(feature = "queue-memory")] // 编译期开启内存后端时
            {
                Ok(std::sync::Arc::new(memory::MemoryQueue::new(settings))) // 构建内存队列并包成句柄
            }
            #[cfg(not(feature = "queue-memory"))] // 未开启内存后端 feature 时
            {
                Err(QueueError::feature_disabled("memory")) // 返回该后端未启用的错误
            }
        }
        "redis" => { // Redis 后端分支
            #[cfg(feature = "queue-redis")] // 编译期开启 redis 后端时
            {
                if !settings.redis.enabled() { // 若 redis 未配置连接地址
                    return Err(QueueError::Config( // 直接返回配置错误
                        "queue.backend = redis but queue.redis.url is empty".to_string(), // 错误说明：url 为空
                    ));
                }
                Ok(std::sync::Arc::new( // 否则异步连接并构建
                    redis::RedisQueue::new(&settings.redis).await?, // 连接 Redis 并包成句柄
                ))
            }
            #[cfg(not(feature = "queue-redis"))] // 未开启 redis 后端 feature 时
            {
                Err(QueueError::Config( // 返回提示需开启对应 feature 的配置错误
                    "queue.backend = redis requires feature queue-redis".to_string(), // 错误说明：需 feature queue-redis
                ))
            }
        }
        "rabbitmq" => { // RabbitMQ 后端分支
            #[cfg(feature = "queue-rabbitmq")] // 编译期开启 rabbitmq 后端时
            {
                Ok(std::sync::Arc::new( // 异步连接并构建
                    rabbitmq::RabbitmqQueue::connect(&settings.rabbitmq).await?, // 连接 RabbitMQ 并包成句柄
                ))
            }
            #[cfg(not(feature = "queue-rabbitmq"))] // 未开启 rabbitmq 后端 feature 时
            {
                Err(QueueError::Config( // 返回提示需开启对应 feature 的配置错误
                    "queue.backend = rabbitmq requires feature queue-rabbitmq".to_string(), // 错误说明：需 feature queue-rabbitmq
                ))
            }
        }
        "kafka" => { // Kafka 后端分支
            #[cfg(feature = "queue-kafka")] // 编译期开启 kafka 后端时
            {
                Ok(std::sync::Arc::new(kafka::KafkaQueue::connect(&settings.kafka).await?)) // 连接 Kafka 并包成句柄
            }
            #[cfg(not(feature = "queue-kafka"))] // 未开启 kafka 后端 feature 时
            {
                Err(QueueError::Config( // 返回提示需开启对应 feature 的配置错误
                    "queue.backend = kafka requires feature queue-kafka".to_string(), // 错误说明：需 feature queue-kafka
                ))
            }
        }
        "nats" => { // NATS 后端分支
            #[cfg(feature = "queue-nats")] // 编译期开启 nats 后端时
            {
                Ok(std::sync::Arc::new(nats::NatsQueue::connect(&settings.nats).await?)) // 连接 NATS 并包成句柄
            }
            #[cfg(not(feature = "queue-nats"))] // 未开启 nats 后端 feature 时
            {
                Err(QueueError::Config( // 返回提示需开启对应 feature 的配置错误
                    "queue.backend = nats requires feature queue-nats".to_string(), // 错误说明：需 feature queue-nats
                ))
            }
        }
        other => Err(QueueError::Config(format!( // 未知后端：格式化并返回配置错误
            "unknown queue.backend: {other} (expected memory / redis / rabbitmq / kafka / nats)" // 列出可选后端清单
        ))),
    }
}

impl QueueError {
    #[allow(dead_code)] // 仅在可选后端 feature 关闭的编译组合中使用
    pub(crate) fn feature_disabled(backend: &str) -> Self { // 构造「后端 feature 未启用」错误
        QueueError::Config(format!( // 以配置错误形式返回
            "queue backend `{backend}` requires its feature to be enabled" // 提示需开启对应 feature
        ))
    }
}
