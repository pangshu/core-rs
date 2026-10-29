//! AMQP 0-9-1 队列后端（`queue-amqp`，RabbitMQ）。
//!
//! 语义映射（与 `memory` 的差异写在每一条里）：
//!
//! | 操作 | 实现 |
//! |---|---|
//! | `enqueue` | 声明 durable 队列（default exchange，routing_key = 队列名）+ **publisher confirm** 的持久化消息 |
//! | `dequeue` | `basic_get` + **手动 ack**（`no_ack = false`），`delivery_tag` 记入进程内映射 |
//! | `ack` | 按映射里的 `delivery_tag` 发 `basic_ack` |
//! | `requeue` | 先 confirm 发布一条新消息，再 `basic_ack` 原消息 |
//! | `dead_letter` | 发布到 `{queue}.dead`（confirm），再 `basic_ack` 原消息；同时记入进程内镜像 |
//! | `dead_letters` | 基于**进程内镜像**（AMQP 没有「只读浏览」API）——**仅供诊断** |
//! | `replay_dead` | 从 broker 的 `{queue}.dead` **re-drive**（`basic_get` → confirm 发布 → `basic_ack`） |
//!
//! **为什么改成手动 ack**：旧实现用 `no_ack = true`（auto-ack），消息一取到 broker 就已
//! 确认——消费端崩溃、业务失败、死信投递失败，消息都**已经不存在于任何地方**，
//! 语义退化为 at-most-once。现在：
//!
//! - 未 ack 的消息留在 broker 的 unacked 集合里，channel 关闭 / 连接断开时 broker 自动
//!   重新投递（真正的 at-least-once）；
//! - `ack` 只拿到 `id`，所以维护 `{queue}:{id} → delivery_tag` 的映射。映射是进程内的，
//!   进程崩溃后丢失 —— 但此时 channel 也断了，broker 会把未确认消息重投，**不会丢**。
//!   映射条目超过 `ack_timeout` 未确认会被清理，避免无限增长。
//!
//! **publisher confirm**：`publish` 等 `Confirmation`，`Nack` 视为失败。没有 confirm 时
//! `enqueue` 的 `Ok` 只代表「已写进 socket」，broker 侧拒绝（队列满、磁盘满）是静默的。
//!
//! 消费端仍需幂等：重投与 `requeue` 都会造成重复投递。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use lapin::options::{
    BasicAckOptions, BasicGetOptions, BasicNackOptions, BasicPublishOptions, ConfirmSelectOptions,
    QueueDeclareOptions,
};
use lapin::publisher_confirm::Confirmation;
use lapin::types::FieldTable;
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::queue::{Message, QueueBackend};

/// 死信镜像上限（仅用于查询/重放，不参与投递语义）。
const MAX_MIRROR: usize = 1000;

/// 一条已投递但未确认的消息。
#[derive(Debug, Clone, Copy)]
struct Unacked {
    tag: u64,
    since: Instant,
}

/// AMQP 队列后端。
pub struct AmqpQueue {
    /// 持有连接：channel 失效时连接仍在，便于重建 channel。
    connection: Connection,
    /// 当前 channel。失效（`connected() == false`）时由 [`AmqpQueue::ensure_channel`] 重建。
    channel: ArcSwap<Channel>,
    /// `{queue}:{id} → delivery_tag`（手动 ack 必需）。
    unacked: Mutex<HashMap<String, Unacked>>,
    /// 死信镜像（进程内，仅用于 `dead_letters` / `replay_dead`）。
    dead: Mutex<VecDeque<Message>>,
    /// 映射条目保留时长（超过则清理，避免消费端不 ack 时无限增长）。
    ack_timeout: Duration,
    /// 清理节流用的计数（每 N 次操作扫一次）。
    ops: AtomicU64,
}

impl std::fmt::Debug for AmqpQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmqpQueue")
            .field("connected", &self.channel.load().status().connected())
            .finish_non_exhaustive()
    }
}

impl AmqpQueue {
    /// 按 AMQP URL 建立连接（`amqp://user:pass@host:5672/%2f`）。
    ///
    /// # Errors
    /// URL 非法或连接失败时返回 `50003`。
    pub async fn connect(url: &str) -> AppResult<Self> {
        let connection = Connection::connect(url, ConnectionProperties::default())
            .await
            .map_err(map_err)?;
        let channel = Self::new_channel(&connection).await?;
        Ok(Self {
            connection,
            channel: ArcSwap::from_pointee(channel),
            unacked: Mutex::new(HashMap::new()),
            dead: Mutex::new(VecDeque::new()),
            ack_timeout: Duration::from_secs(60),
            ops: AtomicU64::new(0),
        })
    }

    /// 设置未确认映射的保留时长（`[queue].visibility_timeout`）。
    #[must_use]
    pub fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.ack_timeout = timeout;
        self
    }

    async fn new_channel(connection: &Connection) -> AppResult<Channel> {
        let channel = connection.create_channel().await.map_err(map_err)?;
        // 打开 publisher confirm：`publish` 才能知道 broker 是否真的收下了。
        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(map_err)?;
        Ok(channel)
    }

    /// 当前 channel（可能已失效）。
    fn channel(&self) -> arc_swap::Guard<std::sync::Arc<Channel>> {
        self.channel.load()
    }

    /// 取一个可用 channel；失效则重建。
    ///
    /// 重建时**清空未确认映射**：旧 channel 上的 `delivery_tag` 全部作废，
    /// broker 会把这些消息重新投递（不会丢），此时再拿旧 tag 去 ack 只会报错。
    async fn ensure_channel(&self) -> AppResult<std::sync::Arc<Channel>> {
        let current = self.channel.load_full();
        if current.status().connected() {
            return Ok(current);
        }

        let fresh = Self::new_channel(&self.connection).await?;
        let fresh = std::sync::Arc::new(fresh);
        self.channel.store(std::sync::Arc::clone(&fresh));
        if let Ok(mut unacked) = self.unacked.lock() {
            unacked.clear();
        }
        tracing::warn!("AMQP channel 已重建；旧 channel 上的未确认消息将由 broker 重新投递");
        Ok(fresh)
    }

    /// 死信队列名。
    fn dead_name(queue: &str) -> String {
        format!("{queue}.dead")
    }

    /// 未确认映射的键（队列名经校验不含 `:`，`id` 是 UUID）。
    fn unacked_key(queue: &str, id: &str) -> String {
        format!("{queue}:{id}")
    }

    async fn declare_on(channel: &Channel, queue: &str) -> AppResult<()> {
        channel
            .queue_declare(
                queue,
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    /// 发布一条消息，并等待 broker 确认。
    ///
    /// `Confirmation::Nack` 视为失败：broker 明确拒绝了这条消息，返回 `Ok` 会让上层
    /// 以为投递成功而丢掉重试机会。
    async fn publish(&self, queue: &str, body: Vec<u8>) -> AppResult<()> {
        let channel = self.ensure_channel().await?;
        Self::declare_on(&channel, queue).await?;

        let confirm = channel
            .basic_publish(
                "",
                queue,
                BasicPublishOptions::default(),
                &body,
                BasicProperties::default().with_delivery_mode(2),
            )
            .await
            .map_err(map_err)?;

        match confirm.await {
            Ok(Confirmation::Ack(_)) | Ok(Confirmation::NotRequested) => Ok(()),
            Ok(Confirmation::Nack(_)) => Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("AMQP broker 拒绝接收消息（nack，队列 {queue}）"),
            )),
            Err(error) => Err(map_err(error)),
        }
    }

    fn remember_dead(&self, message: Message) {
        if let Ok(mut dead) = self.dead.lock() {
            if dead.len() >= MAX_MIRROR {
                dead.pop_front();
            }
            dead.push_back(message);
        }
    }

    /// 定期清理过期的未确认映射条目（消费端拿了消息却永不 ack 时防止内存增长）。
    fn prune_unacked(&self) {
        // 每 64 次操作扫一次，避免每条消息都持锁遍历。
        if !self.ops.fetch_add(1, Ordering::Relaxed).is_multiple_of(64) {
            return;
        }
        let Ok(mut unacked) = self.unacked.lock() else {
            return;
        };
        let timeout = self.ack_timeout;
        let before = unacked.len();
        unacked.retain(|_, entry| entry.since.elapsed() < timeout);
        let dropped = before - unacked.len();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                timeout_ms = timeout.as_millis() as u64,
                "清理超时未确认的 delivery_tag（对应消息会由 broker 重新投递）"
            );
        }
    }

    /// 取走映射里的 `delivery_tag`。
    fn take_unacked(&self, queue: &str, id: &str) -> Option<Unacked> {
        self.unacked
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&Self::unacked_key(queue, id)))
    }
}

#[async_trait]
impl QueueBackend for AmqpQueue {
    async fn enqueue(&self, queue: &str, message: Message) -> AppResult<()> {
        let body = serde_json::to_vec(&message).map_err(encode_err)?;
        self.publish(queue, body).await
    }

    async fn dequeue(&self, queue: &str) -> AppResult<Option<Message>> {
        self.prune_unacked();

        let channel = self.ensure_channel().await?;
        Self::declare_on(&channel, queue).await?;

        let got = channel
            .basic_get(
                queue,
                BasicGetOptions {
                    // 手动 ack：见模块头部的说明
                    no_ack: false,
                },
            )
            .await
            .map_err(map_err)?;

        let Some(delivery) = got else {
            return Ok(None);
        };
        let tag = delivery.delivery.delivery_tag;

        match serde_json::from_slice::<Message>(&delivery.delivery.data) {
            Ok(message) => {
                if let Ok(mut unacked) = self.unacked.lock() {
                    unacked.insert(
                        Self::unacked_key(queue, &message.id),
                        Unacked {
                            tag,
                            since: Instant::now(),
                        },
                    );
                }
                Ok(Some(message))
            }
            Err(error) => {
                tracing::error!(%error, queue, tag, "AMQP 消息解析失败，转入死信");
                let dead = Self::dead_name(queue);
                match self.publish(&dead, delivery.delivery.data.clone()).await {
                    Ok(()) => {
                        // 已落到死信，原消息确认丢弃（否则会被无限重投）。
                        channel
                            .basic_ack(tag, BasicAckOptions::default())
                            .await
                            .map_err(map_err)?;
                    }
                    Err(publish_error) => {
                        // 死信也投不进去：只能丢弃并留痕，requeue 会立刻回到同一个解析失败分支。
                        tracing::error!(
                            %publish_error,
                            queue,
                            tag,
                            "死信投递失败，毒消息被丢弃（请检查 broker 可用性）"
                        );
                        let _ = channel
                            .basic_nack(
                                tag,
                                BasicNackOptions {
                                    requeue: false,
                                    ..Default::default()
                                },
                            )
                            .await;
                    }
                }
                Ok(None)
            }
        }
    }

    async fn ack(&self, queue: &str, id: &str) -> AppResult<()> {
        let Some(entry) = self.take_unacked(queue, id) else {
            // 映射里没有：要么已经 ack 过（重复 ack），要么进程重启后映射丢了。
            // 后者由 broker 重投兜底，这里不能报错（否则消费循环会被一条消息卡住）。
            tracing::debug!(queue, id, "未找到 delivery_tag，跳过 ack");
            return Ok(());
        };

        let channel = self.channel();
        if !channel.status().connected() {
            return Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                "AMQP channel 已断开，ack 未发送（消息将由 broker 重新投递）",
            ));
        }
        channel
            .basic_ack(entry.tag, BasicAckOptions::default())
            .await
            .map_err(map_err)
    }

    async fn requeue(&self, queue: &str, message: Message) -> AppResult<()> {
        let body = serde_json::to_vec(&message).map_err(encode_err)?;
        // 先发新消息（confirm 通过），再确认旧的：顺序反了会在「确认成功、发布失败」时丢消息。
        self.publish(queue, body).await?;
        self.ack(queue, &message.id).await
    }

    async fn dead_letter(&self, queue: &str, message: Message) -> AppResult<()> {
        let body = serde_json::to_vec(&message).map_err(encode_err)?;
        self.publish(&Self::dead_name(queue), body).await?;
        self.remember_dead(message.clone());
        self.ack(queue, &message.id).await
    }

    async fn len(&self, queue: &str) -> AppResult<usize> {
        let channel = self.ensure_channel().await?;
        let declared = channel
            .queue_declare(
                queue,
                QueueDeclareOptions {
                    passive: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(map_err)?;
        Ok(declared.message_count() as usize)
    }

    async fn dead_letters(&self, queue: &str) -> AppResult<Vec<Message>> {
        let _ = queue;
        let dead = self.dead.lock().map_err(|_| poisoned())?;
        // 只用于诊断：镜像按实例分片、最多 `MAX_MIRROR` 条，且不含历史。
        // 重放**不**读镜像（见 `replay_dead`），否则会把仍在 broker 死信队列里的
        // 消息再投一遍。
        Ok(dead.iter().cloned().collect())
    }

    async fn replay_dead(&self, queue: &str, max: usize) -> AppResult<usize> {
        if max == 0 {
            return Ok(0);
        }
        let channel = self.ensure_channel().await?;
        let dead = Self::dead_name(queue);
        Self::declare_on(&channel, &dead).await?;

        // 从 broker 的 `{queue}.dead` 里 re-drive，而不是从进程内镜像：
        // 镜像按实例分片，用它重放会把「仍躺在 broker 死信队列里」的消息再投一遍
        // （重复消费），且多实例各放各的、永远对不齐。
        let mut replayed = 0;
        for _ in 0..max {
            let got = channel
                .basic_get(&dead, BasicGetOptions { no_ack: false })
                .await
                .map_err(map_err)?;
            let Some(delivery) = got else {
                break;
            };
            let tag = delivery.delivery.delivery_tag;

            match serde_json::from_slice::<Message>(&delivery.delivery.data) {
                Ok(mut message) => {
                    // 与 `memory` 对齐：重放视为重新投递，复位次数，否则下一次失败立刻又进死信。
                    message.attempts = 0;
                    message.queue = queue.to_string();
                    let body = serde_json::to_vec(&message).map_err(encode_err)?;

                    match self.publish(queue, body).await {
                        Ok(()) => {
                            channel
                                .basic_ack(tag, BasicAckOptions::default())
                                .await
                                .map_err(map_err)?;
                            replayed += 1;
                        }
                        Err(error) => {
                            // 主队列投不进去：把这条死信退回，别让它卡在 unacked 里
                            // 等 channel 关闭（那期间它既不在 dead 也不在 ready）。
                            let _ = channel
                                .basic_nack(
                                    tag,
                                    BasicNackOptions {
                                        requeue: true,
                                        ..Default::default()
                                    },
                                )
                                .await;
                            return Err(error);
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(%error, queue, "死信消息解析失败，丢弃");
                    let _ = channel
                        .basic_nack(
                            tag,
                            BasicNackOptions {
                                requeue: false,
                                ..Default::default()
                            },
                        )
                        .await;
                }
            }
        }
        Ok(replayed)
    }

    async fn ping(&self) -> AppResult<()> {
        if !self.connection.status().connected() {
            return Err(AppError::biz(ErrorCode::UPSTREAM_ERROR, "AMQP 连接已断开"));
        }
        // 只看连接状态会漏掉「连接在、channel 被 broker 关掉」——此时所有操作都失败，
        // 但 /ready 仍是绿的。channel 失效视为不健康，由重建逻辑恢复。
        if !self.channel().status().connected() {
            return Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                "AMQP channel 已断开（连接仍在）",
            ));
        }
        Ok(())
    }
}

fn map_err(error: lapin::Error) -> AppError {
    tracing::error!(%error, "AMQP 队列访问失败");
    AppError::biz(ErrorCode::UPSTREAM_ERROR, "队列后端不可用")
}

fn encode_err(error: serde_json::Error) -> AppError {
    AppError::biz(
        ErrorCode::UPSTREAM_ERROR,
        format!("队列消息编解码失败：{error}"),
    )
}

fn poisoned() -> AppError {
    AppError::internal(anyhow::anyhow!("AMQP 死信镜像锁已中毒"))
}
