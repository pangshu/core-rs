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

mod build; // 按 [queue] 配置构建队列实例
mod contract; // 队列契约（Queue）与投递结构
mod error; // 队列统一错误类型

pub use build::build; // 对外导出队列构建入口
pub use contract::{Delivery, Queue, QueueHandle}; // 对外导出队列契约、投递结构与共享句柄
pub use error::QueueError; // 对外导出队列错误类型
pub use message::Message; // 对外重导出消息类型，使用方无需关心子模块路径
