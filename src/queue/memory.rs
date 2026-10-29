//! 进程内队列后端（`queue-memory`，默认）。
//!
//! 语义：`enqueue` → ready；`dequeue` 移到 **in-flight**；`ack` 删除；
//! `requeue` / `dead_letter` 显式处置。分区内**有序**（FIFO），跨队列无序。
//!
//! ⚠️ 局限：in-flight 只存在于进程内——进程崩溃会丢失未 ack 的消息，
//! 也没有可见性超时自动重投。生产多实例请切 `queue-redis` / `queue-amqp` /
//! `queue-kafka`（见 `docs/03` §4.12）。
//!
//! 深度上限由 `[queue].max_depth` 控制：消费端停摆时，无限入队会直接吃满进程内存。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::queue::{Message, QueueBackend};

/// 默认单队列深度上限（`[queue].max_depth` 的默认值）。
pub const DEFAULT_MAX_DEPTH: usize = 100_000;

#[derive(Default)]
struct Inner {
    ready: HashMap<String, VecDeque<Message>>,
    inflight: HashMap<String, Message>,
    dead: HashMap<String, VecDeque<Message>>,
}

/// 进程内队列。
pub struct MemoryQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    /// 单队列 ready + dead 的深度上限（防止消费端停摆时 OOM）。
    max_depth: usize,
}

impl Default for MemoryQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MemoryQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryQueue").finish_non_exhaustive()
    }
}

impl MemoryQueue {
    /// 新建空队列集（深度上限取 [`DEFAULT_MAX_DEPTH`]）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    /// 设置单队列深度上限（0 视为不限）。
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// 等待有新消息入队（供消费循环降低空转）。
    ///
    /// 用 `notify_one`（会**保留一个许可**）而不是 `notify_waiters`（不保留）：
    /// 消息在「`dequeue` 返回空」与「`notified()` 注册」之间入队时，后者会丢掉这次
    /// 唤醒，调用方永久睡死；前者会让下一次 `notified()` 立即就绪。
    ///
    /// 仍然建议写成「查-等-再查」结构（每次等待后都重新查一次），这样即使拿到的是
    /// 别的队列的唤醒（虚假唤醒）也不会卡住。
    pub async fn wait_for_message(&self) {
        self.notify.notified().await;
    }
}

#[async_trait]
impl QueueBackend for MemoryQueue {
    async fn enqueue(&self, queue: &str, message: Message) -> AppResult<()> {
        {
            let mut inner = self.lock()?;
            let ready = inner.ready.entry(queue.to_string()).or_default();
            // 消费端停摆时无限入队会吃满进程内存；这里宁可让生产者看到 503。
            if self.max_depth > 0 && ready.len() >= self.max_depth {
                return Err(AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    format!(
                        "内存队列 {queue} 已达深度上限 {}（[queue].max_depth）",
                        self.max_depth
                    ),
                ));
            }
            ready.push_back(message);
        }
        self.notify.notify_one();
        Ok(())
    }

    async fn dequeue(&self, queue: &str) -> AppResult<Option<Message>> {
        let mut inner = self.lock()?;
        let Some(message) = inner
            .ready
            .get_mut(queue)
            .and_then(std::collections::VecDeque::pop_front)
        else {
            return Ok(None);
        };
        inner.inflight.insert(message.id.clone(), message.clone());
        Ok(Some(message))
    }

    async fn ack(&self, _queue: &str, id: &str) -> AppResult<()> {
        self.lock()?.inflight.remove(id);
        Ok(())
    }

    async fn requeue(&self, queue: &str, message: Message) -> AppResult<()> {
        {
            let mut inner = self.lock()?;
            inner.inflight.remove(&message.id);
            inner
                .ready
                .entry(queue.to_string())
                .or_default()
                .push_back(message);
        }
        self.notify.notify_one();
        Ok(())
    }

    async fn dead_letter(&self, queue: &str, message: Message) -> AppResult<()> {
        let mut inner = self.lock()?;
        inner.inflight.remove(&message.id);
        let dead = inner.dead.entry(queue.to_string()).or_default();
        // 死信不能反过来阻塞消费：满了就丢最旧的，并留痕。
        if self.max_depth > 0 && dead.len() >= self.max_depth {
            dead.pop_front();
            tracing::warn!(
                queue,
                limit = self.max_depth,
                "内存死信队列已达上限，丢弃最旧的一条"
            );
        }
        dead.push_back(message);
        Ok(())
    }

    async fn len(&self, queue: &str) -> AppResult<usize> {
        Ok(self
            .lock()?
            .ready
            .get(queue)
            .map_or(0, std::collections::VecDeque::len))
    }

    async fn dead_letters(&self, queue: &str) -> AppResult<Vec<Message>> {
        Ok(self
            .lock()?
            .dead
            .get(queue)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default())
    }

    async fn replay_dead(&self, queue: &str, max: usize) -> AppResult<usize> {
        let replayed = {
            let mut inner = self.lock()?;
            let mut moved = Vec::new();
            if let Some(dead) = inner.dead.get_mut(queue) {
                for _ in 0..max {
                    let Some(mut message) = dead.pop_front() else {
                        break;
                    };
                    // 重放视为重新投递：重置次数，避免立刻再次入死信。
                    message.attempts = 0;
                    moved.push(message);
                }
            }
            let replayed = moved.len();
            inner
                .ready
                .entry(queue.to_string())
                .or_default()
                .extend(moved);
            replayed
        };
        if replayed > 0 {
            self.notify.notify_one();
        }
        Ok(replayed)
    }
}

impl MemoryQueue {
    fn lock(&self) -> AppResult<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| AppError::biz(ErrorCode::UPSTREAM_ERROR, "内存队列状态被污染"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> Message {
        Message::new("orders", id.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn should_dequeue_in_fifo_order() {
        let queue = MemoryQueue::new();
        queue.enqueue("orders", msg("a")).await.unwrap();
        queue.enqueue("orders", msg("b")).await.unwrap();

        let first = queue.dequeue("orders").await.unwrap().expect("a");
        let second = queue.dequeue("orders").await.unwrap().expect("b");
        assert_eq!(first.payload, b"a");
        assert_eq!(second.payload, b"b");
        assert!(queue.dequeue("orders").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_not_redeliver_after_ack() {
        let queue = MemoryQueue::new();
        queue.enqueue("orders", msg("a")).await.unwrap();
        let message = queue.dequeue("orders").await.unwrap().expect("msg");
        queue.ack("orders", &message.id).await.unwrap();
        assert!(queue.dequeue("orders").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_redeliver_after_requeue() {
        let queue = MemoryQueue::new();
        queue.enqueue("orders", msg("a")).await.unwrap();
        let mut message = queue.dequeue("orders").await.unwrap().expect("msg");
        message.attempts += 1;
        queue.requeue("orders", message).await.unwrap();

        let again = queue.dequeue("orders").await.unwrap().expect("requeued");
        assert_eq!(again.attempts, 1);
        assert_eq!(queue.len("orders").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn should_move_to_dead_letters() {
        let queue = MemoryQueue::new();
        queue.enqueue("orders", msg("a")).await.unwrap();
        let message = queue.dequeue("orders").await.unwrap().expect("msg");
        queue.dead_letter("orders", message).await.unwrap();

        let dead = queue.dead_letters("orders").await.unwrap();
        assert_eq!(dead.len(), 1);
        assert!(queue.dequeue("orders").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_replay_dead_letters_with_reset_attempts() {
        let queue = MemoryQueue::new();
        queue.enqueue("orders", msg("a")).await.unwrap();
        let mut message = queue.dequeue("orders").await.unwrap().expect("msg");
        message.attempts = 5;
        queue.dead_letter("orders", message).await.unwrap();

        assert_eq!(queue.replay_dead("orders", 10).await.unwrap(), 1);
        assert!(queue.dead_letters("orders").await.unwrap().is_empty());

        let replayed = queue.dequeue("orders").await.unwrap().expect("replayed");
        assert_eq!(replayed.attempts, 0, "重放应重置投递次数");
    }

    #[tokio::test]
    async fn should_isolate_queues() {
        let queue = MemoryQueue::new();
        queue.enqueue("a", msg("1")).await.unwrap();
        queue.enqueue("b", msg("2")).await.unwrap();

        assert_eq!(queue.len("a").await.unwrap(), 1);
        assert_eq!(queue.len("b").await.unwrap(), 1);
        assert!(queue.dequeue("c").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_reject_enqueue_beyond_max_depth() {
        let queue = MemoryQueue::new().with_max_depth(1);
        queue.enqueue("orders", msg("a")).await.unwrap();

        let error = queue
            .enqueue("orders", msg("b"))
            .await
            .expect_err("超过深度上限应报错");
        assert_eq!(
            error.code(),
            crate::errors::ErrorCode::SERVICE_UNAVAILABLE,
            "队列满应映射 503 而不是静默吃内存"
        );
        assert_eq!(queue.len("orders").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn should_trim_dead_letters_at_max_depth() {
        let queue = MemoryQueue::new().with_max_depth(1);

        queue.enqueue("orders", msg("a")).await.unwrap();
        let first = queue.dequeue("orders").await.unwrap().expect("msg");
        queue.dead_letter("orders", first).await.unwrap();

        queue.enqueue("orders", msg("b")).await.unwrap();
        let second = queue.dequeue("orders").await.unwrap().expect("msg");
        queue.dead_letter("orders", second).await.unwrap();

        let dead = queue.dead_letters("orders").await.unwrap();
        assert_eq!(dead.len(), 1, "死信满了应丢最旧的，而不是无限增长");
        assert_eq!(dead[0].payload, b"b");
    }
}
