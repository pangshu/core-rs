//! 队列后端抽象（`memory` / `redis` / `amqp` / `kafka` 各自实现）。

use async_trait::async_trait;

use crate::errors::AppResult;
use crate::queue::Message;

/// 队列后端抽象（object-safe）。
///
/// 语义：`dequeue` 把消息移入 **in-flight**，由 `ack` / `requeue` / `dead_letter`
/// 之一终结；未终结的消息在后端重启时可能丢失（`memory`）。
#[async_trait]
pub trait QueueBackend: Send + Sync + 'static {
    /// 入队。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`（`UPSTREAM_ERROR`）。
    async fn enqueue(&self, queue: &str, message: Message) -> AppResult<()>;

    /// 取一条消息（移入 in-flight）；队列为空返回 `Ok(None)`。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn dequeue(&self, queue: &str) -> AppResult<Option<Message>>;

    /// 确认消费成功（从 in-flight 移除）。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn ack(&self, queue: &str, id: &str) -> AppResult<()>;

    /// 重新入队（重试）。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn requeue(&self, queue: &str, message: Message) -> AppResult<()>;

    /// 移入死信队列。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn dead_letter(&self, queue: &str, message: Message) -> AppResult<()>;

    /// 待消费消息数。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn len(&self, queue: &str) -> AppResult<usize>;

    /// 死信列表。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn dead_letters(&self, queue: &str) -> AppResult<Vec<Message>>;

    /// 重放死信（最多 `max` 条），返回重放条数。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn replay_dead(&self, queue: &str, max: usize) -> AppResult<usize>;

    /// 连通性探测（供 `/ready`）。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    async fn ping(&self) -> AppResult<()> {
        Ok(())
    }
}
