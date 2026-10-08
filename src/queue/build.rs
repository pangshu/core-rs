//! 按 `[queue]` 配置构建队列实例（App 装配时自动调用；手动装配亦可用）。
//! 后端建连失败（坏地址）在装配期报错。

use crate::config::sections::QueueSettings; // 引入 `[queue]` 配置段类型，供 build 读取

use super::{QueueError, QueueHandle}; // 引入队列错误类型与共享句柄

/// 按 `[queue]` 配置构建队列实例（App 装配时自动调用；手动装配亦可用）。
/// 后端建连失败（坏地址）在装配期报错。
pub async fn build(settings: &QueueSettings) -> Result<QueueHandle, QueueError> { // 依据配置构建并返回队列句柄
    match settings.backend.as_str() { // 按配置的 backend 字符串分派
        "memory" => { // 内存后端分支
            #[cfg(feature = "queue-memory")] // 编译期开启内存后端时
            {
                Ok(std::sync::Arc::new(super::memory::MemoryQueue::new(settings))) // 构建内存队列并包成句柄
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
                    super::redis::RedisQueue::new(&settings.redis).await?, // 连接 Redis 并包成句柄
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
                    super::rabbitmq::RabbitmqQueue::connect(&settings.rabbitmq).await?, // 连接 RabbitMQ 并包成句柄
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
                Ok(std::sync::Arc::new(super::kafka::KafkaQueue::connect(&settings.kafka).await?)) // 连接 Kafka 并包成句柄
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
                Ok(std::sync::Arc::new(super::nats::NatsQueue::connect(&settings.nats).await?)) // 连接 NATS 并包成句柄
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
