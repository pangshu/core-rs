//! 队列（feature `queue`）。
//!
//! - [`QueueBackend`]：object-safe 的后端抽象；`memory` / `redis` / `amqp` / `kafka` 各自实现
//! - [`QueueService`]：装配入口（`AppState.queue`），按 `[queue].backend` 选后端
//! - [`Producer`] / [`Consumer`]：生产与消费句柄（消费含重试、死信、幂等去重）
//! - [`Message`]：消息结构体，`id` 即**消费幂等键**
//!
//! 语义差异见各后端模块文档（redis 可精确 ack；amqp auto-ack；kafka auto-commit）。
//!
//! 文件划分：`message`（结构体）/ `backend`（trait）/ `service`（装配）/ `health`（探针）/
//! `producer`·`consumer`·`dlq` / 各后端实现。

#[cfg(feature = "queue-amqp")]
pub mod amqp;
mod backend;
pub mod consumer;
pub mod dlq;
mod health;
#[cfg(feature = "queue-kafka")]
pub mod kafka;
pub mod memory;
mod message;
pub mod producer;
#[cfg(feature = "queue-redis")]
pub mod redis;
mod service;

pub use backend::QueueBackend;
pub use consumer::Consumer;
pub use health::QueueHealth;
pub use memory::MemoryQueue;
pub use message::Message;
pub use producer::Producer;
pub use service::{QueueService, validate_queue_name};

pub(crate) use service::IDLE_POLL_INTERVAL;
