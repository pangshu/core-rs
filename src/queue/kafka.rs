//! Kafka 队列后端（`queue-kafka`）。
//!
//! 语义映射（Kafka 没有「单条 ack」，这里的取舍必须写清楚）：
//!
//! | 操作 | 实现 |
//! |---|---|
//! | `enqueue` | `FutureProducer::send` 到 topic（key = 消息 id，保证同 id 同分区） |
//! | `dequeue` | 每个队列惰性建一个 `BaseConsumer`（`group.id = aurora-{queue}`）后 `poll`，记录 `(topic, partition, offset)` |
//! | `ack` | **手动提交**该消息的 `offset + 1`（`enable.auto.commit = false`） |
//! | `requeue` | 重新发布（`attempts` 由上层递增）后提交原 offset |
//! | `dead_letter` | 发布到 `{queue}.dead` 后提交原 offset |
//! | `dead_letters` | **只读窥视** `{queue}.dead`：临时消费者 `assign` + 从头读，不改任何状态 |
//! | `replay_dead` | 用独立消费组 `aurora-{queue}-dead-replay` 从 `{queue}.dead` 读出 → 重投主 topic → 提交 offset |
//! | `len` | 分区 0 的 `high - low` 水位差（多分区时是近似值；不再为此创建消费者） |
//!
//! **为什么手动提交**：`enable.auto.commit = true` 提交的是 **poll 位置**而不是「处理完成
//! 的位置」，默认 5s 提交一次——消费端在处理中途崩溃时，offset 可能已经提交，消息就再也
//! 不会重投了。语义不是文档宣称的 at-least-once，而是**事实上的 at-most-once**。
//! 现在：`enable.auto.commit = false` + `enable.auto.offset.store = false`，只在
//! `ack` / `requeue` / `dead_letter` 这些**终结操作**里提交 `offset + 1`。
//!
//! 提交是 `CommitMode::Async`（不阻塞 async worker）；失败由 librdkafka 记日志，
//! 最坏结果是重投一次——消费端仍需幂等（`[queue].idempotency`）。
//!
//! 崩溃语义：`pending`（`id → offset`）是进程内映射，崩溃后丢失；但 offset 没提交，
//! 重启后会从上次提交处继续消费，**不会丢消息**，只会重复。
//!
//! 死信不再有「进程内镜像」：`{queue}.dead` 就是唯一事实来源，查询走只读窥视、
//! 重放走独立消费组，两者都不会因为多实例 / 重启而看到不同的东西，也不会重复投递。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer};
use rdkafka::message::Message as KafkaMessage;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use rdkafka::util::Timeout;
use rdkafka::{Offset, TopicPartitionList};

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::queue::{Message, QueueBackend};

/// 单次发送 / 元数据请求超时。
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// 单次 `poll` 的最长等待（只在窥视 / 重放这类管理操作里用）。
const POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// 连续多少次空 `poll` 视为「读完了」。
const EMPTY_POLLS: usize = 2;
/// 每个后端实例最多缓存多少个消费者。
///
/// 消费者是「每队列一个」惰性创建的（各带一个线程 + socket）。队列名若来自
/// 不受控输入，不设上限就是 fd / 线程泄漏。超出时直接报错，让调用方尽早发现
/// 「队列名不该动态生成」。
const MAX_CONSUMERS: usize = 256;
/// 单次 `dead_letters` / `replay_dead` 的默认条数上限（防一次管理调用拉爆内存）。
const MAX_PEEK: usize = 1000;

/// 一条已 poll 出、尚未终结的消息在 Kafka 里的位置。
#[derive(Debug, Clone)]
struct Pending {
    topic: String,
    partition: i32,
    offset: i64,
    since: Instant,
}

/// Kafka 队列后端。
pub struct KafkaQueue {
    producer: FutureProducer,
    brokers: String,
    /// 每个队列一个消费者（惰性创建并复用，避免每次 `dequeue` 重新订阅）。
    consumers: Mutex<HashMap<String, Arc<BaseConsumer>>>,
    /// `{queue}:{id} → (topic, partition, offset)`，手动提交必需。
    pending: Mutex<HashMap<String, Pending>>,
    /// 映射条目保留时长（超过则清理，避免消费端不 ack 时无限增长）。
    ack_timeout: Duration,
    /// 清理节流用的计数。
    ops: AtomicU64,
}

impl std::fmt::Debug for KafkaQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaQueue")
            .field("brokers", &self.brokers)
            .finish_non_exhaustive()
    }
}

impl KafkaQueue {
    /// 按 broker 列表建立生产者。
    ///
    /// # Errors
    /// 配置非法或生产者创建失败时返回 `50003`。
    pub fn connect(brokers: &[String]) -> AppResult<Self> {
        if brokers.is_empty() {
            return Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                "[queue].kafka.brokers 不能为空",
            ));
        }
        let brokers = brokers.join(",");
        let producer = ClientConfig::new()
            .set("bootstrap.servers", &brokers)
            .set("message.timeout.ms", "5000")
            .create::<FutureProducer>()
            .map_err(|error| {
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    format!("Kafka 生产者创建失败：{error}"),
                )
            })?;
        Ok(Self {
            producer,
            brokers,
            consumers: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            ack_timeout: Duration::from_secs(60),
            ops: AtomicU64::new(0),
        })
    }

    /// 设置未提交映射的保留时长（`[queue].visibility_timeout`）。
    #[must_use]
    pub fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.ack_timeout = timeout;
        self
    }

    fn dead_topic(queue: &str) -> String {
        format!("{queue}.dead")
    }

    fn pending_key(queue: &str, id: &str) -> String {
        format!("{queue}:{id}")
    }

    /// 建一个不参与消费组的临时消费者（只读窥视 / 重放用）。
    fn ephemeral(&self, group: &str) -> AppResult<BaseConsumer> {
        ClientConfig::new()
            .set("bootstrap.servers", &self.brokers)
            .set("group.id", group)
            .set("enable.auto.commit", "false")
            .set("enable.auto.offset.store", "false")
            .set("auto.offset.reset", "earliest")
            .create::<BaseConsumer>()
            .map_err(|error| {
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    format!("Kafka 临时消费者创建失败：{error}"),
                )
            })
    }

    /// 取（或创建）该队列的消费者。
    fn consumer_for(&self, queue: &str) -> AppResult<Arc<BaseConsumer>> {
        let mut guard = self
            .consumers
            .lock()
            .map_err(|_| AppError::internal(anyhow::anyhow!("Kafka 消费者表锁已中毒")))?;
        if let Some(existing) = guard.get(queue) {
            return Ok(Arc::clone(existing));
        }
        if guard.len() >= MAX_CONSUMERS {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!(
                    "Kafka 消费者数量已达上限 {MAX_CONSUMERS}；队列名不应动态生成（见 validate_queue_name）"
                ),
            ));
        }

        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &self.brokers)
            .set("group.id", format!("aurora-{queue}"))
            // 手动提交：见模块头部的说明
            .set("enable.auto.commit", "false")
            .set("enable.auto.offset.store", "false")
            .set("auto.offset.reset", "earliest")
            .create()
            .map_err(|error| {
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    format!("Kafka 消费者创建失败：{error}"),
                )
            })?;
        consumer.subscribe(&[queue]).map_err(|error| {
            AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 订阅 {queue} 失败：{error}"),
            )
        })?;

        let consumer = Arc::new(consumer);
        guard.insert(queue.to_string(), Arc::clone(&consumer));
        Ok(consumer)
    }

    async fn publish(&self, topic: &str, message: &Message) -> AppResult<()> {
        let body = serde_json::to_vec(message).map_err(encode_err)?;
        self.producer
            .send(
                FutureRecord::to(topic).payload(&body).key(&message.id),
                Timeout::After(IO_TIMEOUT),
            )
            .await
            .map_err(|(error, _)| {
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    format!("Kafka 投递失败：{error}"),
                )
            })?;
        Ok(())
    }

    /// 提交 `(topic, partition, offset + 1)`：offset 语义是「下一条要读的位置」。
    fn commit_offset(&self, queue: &str, pending: &Pending) -> AppResult<()> {
        let consumer = self.consumer_for(queue)?;
        let mut list = TopicPartitionList::new();
        list.add_partition_offset(
            &pending.topic,
            pending.partition,
            Offset::Offset(pending.offset.saturating_add(1)),
        )
        .map_err(|error| {
            AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 提交位点失败：{error}"),
            )
        })?;
        consumer.commit(&list, CommitMode::Async).map_err(|error| {
            AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 提交位点失败：{error}"),
            )
        })
    }

    /// 取走 `pending` 里的位置并提交。
    fn finish(&self, queue: &str, id: &str) -> AppResult<()> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| AppError::internal(anyhow::anyhow!("Kafka 未提交映射锁已中毒")))?
            .remove(&Self::pending_key(queue, id));

        match pending {
            Some(pending) => self.commit_offset(queue, &pending),
            None => {
                // 映射丢了（进程重启 / 已终结过）：不提交，让位点自然前移即可。
                tracing::debug!(queue, id, "未找到未提交位点，跳过 offset 提交");
                Ok(())
            }
        }
    }

    /// 定期清理过期的未提交映射条目。
    fn prune_pending(&self) {
        if !self.ops.fetch_add(1, Ordering::Relaxed).is_multiple_of(64) {
            return;
        }
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let timeout = self.ack_timeout;
        let before = pending.len();
        pending.retain(|_, entry| entry.since.elapsed() < timeout);
        let dropped = before - pending.len();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                timeout_ms = timeout.as_millis() as u64,
                "清理超时未提交的位点（对应消息在重启 / 再均衡后会重新投递）"
            );
        }
    }
}

#[async_trait]
impl QueueBackend for KafkaQueue {
    async fn enqueue(&self, queue: &str, message: Message) -> AppResult<()> {
        self.publish(queue, &message).await
    }

    async fn dequeue(&self, queue: &str) -> AppResult<Option<Message>> {
        self.prune_pending();

        let consumer = self.consumer_for(queue)?;

        // `poll` 是阻塞调用，放到 blocking 池，避免占住 async worker
        let polled = tokio::task::spawn_blocking(move || {
            consumer.poll(Duration::from_millis(0)).map(|result| {
                result.map(|borrowed| {
                    (
                        borrowed.payload().unwrap_or_default().to_vec(),
                        borrowed.topic().to_string(),
                        borrowed.partition(),
                        borrowed.offset(),
                    )
                })
            })
        })
        .await
        .map_err(|error| AppError::internal(anyhow::anyhow!("Kafka poll 任务失败：{error}")))?;

        let Some(result) = polled else {
            return Ok(None);
        };
        let (payload, topic, partition, offset) = result.map_err(|error| {
            AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 消费失败：{error}"),
            )
        })?;

        match serde_json::from_slice::<Message>(&payload) {
            Ok(message) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.insert(
                        Self::pending_key(queue, &message.id),
                        Pending {
                            topic,
                            partition,
                            offset,
                            since: Instant::now(),
                        },
                    );
                }
                Ok(Some(message))
            }
            Err(error) => {
                // 解析不了的消息不能只记一行日志就扔：那和「静默丢数据」没区别。
                // 转死信后提交位点；死信也投不进去时不提交，重启后会重试。
                tracing::error!(%error, queue, "Kafka 消息解析失败，转入死信");
                let raw = Message {
                    id: uuid::Uuid::new_v4().to_string(),
                    queue: queue.to_string(),
                    payload,
                    attempts: 0,
                    enqueued_at: chrono::Utc::now(),
                };
                self.publish(&Self::dead_topic(queue), &raw).await?;

                let pending = Pending {
                    topic,
                    partition,
                    offset,
                    since: Instant::now(),
                };
                self.commit_offset(queue, &pending)?;
                Ok(None)
            }
        }
    }

    async fn ack(&self, queue: &str, id: &str) -> AppResult<()> {
        self.finish(queue, id)
    }

    async fn requeue(&self, queue: &str, message: Message) -> AppResult<()> {
        // 先发新消息，再提交原位点：顺序反了会在「提交成功、发布失败」时丢消息。
        self.publish(queue, &message).await?;
        self.finish(queue, &message.id)
    }

    async fn dead_letter(&self, queue: &str, message: Message) -> AppResult<()> {
        self.publish(&Self::dead_topic(queue), &message).await?;
        self.finish(queue, &message.id)
    }

    async fn len(&self, queue: &str) -> AppResult<usize> {
        let topic = queue.to_string();
        let watermarks =
            self.producer
                .client()
                .fetch_watermarks(&topic, 0, Timeout::After(IO_TIMEOUT));

        match watermarks {
            Ok((low, high)) => Ok(usize::try_from((high - low).max(0)).unwrap_or(0)),
            // topic 尚不存在（还没投递过）视为空队列
            Err(rdkafka::error::KafkaError::MetadataFetch(
                rdkafka::types::RDKafkaErrorCode::UnknownTopicOrPartition,
            )) => Ok(0),
            Err(error) => Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 水位查询失败：{error}"),
            )),
        }
    }

    async fn dead_letters(&self, queue: &str) -> AppResult<Vec<Message>> {
        let brokers = self.brokers.clone();
        let queue = queue.to_string();
        tokio::task::spawn_blocking(move || peek_dead(&brokers, &queue, MAX_PEEK))
            .await
            .map_err(|error| AppError::internal(anyhow::anyhow!("Kafka 死信窥视失败：{error}")))?
    }

    async fn replay_dead(&self, queue: &str, max: usize) -> AppResult<usize> {
        if max == 0 {
            return Ok(0);
        }
        let topic = Self::dead_topic(queue);
        let consumer = Arc::new(self.ephemeral(&format!("aurora-{queue}-dead-replay"))?);

        // 1) 阻塞读：把死信读进内存（最多 max 条）。
        let reader = Arc::clone(&consumer);
        let read_topic = topic.clone();
        let batch = tokio::task::spawn_blocking(move || {
            reader.subscribe(&[&read_topic]).map_err(|error| {
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    format!("Kafka 订阅死信 topic 失败：{error}"),
                )
            })?;

            let mut out = Vec::new();
            let mut empty = 0;
            while out.len() < max && empty < EMPTY_POLLS {
                match reader.poll(POLL_TIMEOUT) {
                    Some(Ok(borrowed)) => {
                        empty = 0;
                        match serde_json::from_slice::<Message>(
                            borrowed.payload().unwrap_or_default(),
                        ) {
                            Ok(message) => {
                                out.push((message, borrowed.partition(), borrowed.offset()))
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Kafka 死信解析失败，跳过");
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "Kafka 死信重放读取失败");
                        empty += 1;
                    }
                    None => empty += 1,
                }
            }
            Ok::<_, AppError>(out)
        })
        .await
        .map_err(|error| AppError::internal(anyhow::anyhow!("Kafka 死信重放失败：{error}")))??;

        // 2) 逐条重投主 topic，成功的才推进位点（失败即停，下次从断点继续）。
        let mut committed = TopicPartitionList::new();
        let mut replayed = 0usize;
        for (mut message, partition, offset) in batch {
            // 与 `memory` 对齐：重放视为重新投递，复位次数，否则下一次失败立刻又进死信。
            message.attempts = 0;
            message.queue = queue.to_string();
            self.publish(queue, &message).await?;
            committed
                .add_partition_offset(&topic, partition, Offset::Offset(offset.saturating_add(1)))
                .map_err(|error| {
                    AppError::biz(
                        ErrorCode::UPSTREAM_ERROR,
                        format!("Kafka 位点设置失败：{error}"),
                    )
                })?;
            replayed += 1;
        }

        if replayed > 0 {
            consumer
                .commit(&committed, CommitMode::Async)
                .map_err(|error| {
                    AppError::biz(
                        ErrorCode::UPSTREAM_ERROR,
                        format!("Kafka 提交重放位点失败：{error}"),
                    )
                })?;
        }
        Ok(replayed)
    }

    async fn ping(&self) -> AppResult<()> {
        self.producer
            .client()
            .fetch_metadata(None, Timeout::After(IO_TIMEOUT))
            .map_err(|error| {
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    format!("Kafka 元数据查询失败：{error}"),
                )
            })?;
        Ok(())
    }
}

fn encode_err(error: serde_json::Error) -> AppError {
    AppError::biz(
        ErrorCode::UPSTREAM_ERROR,
        format!("队列消息编解码失败：{error}"),
    )
}

/// 只读窥视死信 topic：`assign` 到所有分区的最早位点，读最多 `max` 条。
///
/// 用 `assign` 而不是 `subscribe`：不加入消费组、不写 `__consumer_offsets`，
/// 每次调用都从最早的位点重新读，天然可重复、无副作用。
///
/// 独立函数（非方法）：要在 `spawn_blocking` 里跑，闭包必须 `'static`。
fn peek_dead(brokers: &str, queue: &str, max: usize) -> AppResult<Vec<Message>> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let topic = KafkaQueue::dead_topic(queue);
    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", format!("aurora-{queue}-dead-peek"))
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .map_err(|error| {
            AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 死信窥视消费者创建失败：{error}"),
            )
        })?;

    // topic 不存在（还没产生过死信）时返回空，而不是报错。
    let metadata = match consumer.fetch_metadata(Some(&topic), Timeout::After(IO_TIMEOUT)) {
        Ok(metadata) => metadata,
        Err(rdkafka::error::KafkaError::MetadataFetch(
            rdkafka::types::RDKafkaErrorCode::UnknownTopicOrPartition,
        )) => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Kafka 元数据查询失败：{error}"),
            ));
        }
    };

    let mut list = TopicPartitionList::new();
    for topic_meta in metadata.topics().iter().filter(|t| t.name() == topic) {
        for partition in topic_meta.partitions() {
            list.add_partition_offset(&topic, partition.id(), Offset::Beginning)
                .map_err(|error| {
                    AppError::biz(
                        ErrorCode::UPSTREAM_ERROR,
                        format!("Kafka 位点设置失败：{error}"),
                    )
                })?;
        }
    }
    if list.count() == 0 {
        return Ok(Vec::new());
    }
    consumer.assign(&list).map_err(|error| {
        AppError::biz(
            ErrorCode::UPSTREAM_ERROR,
            format!("Kafka 分区分配失败：{error}"),
        )
    })?;

    let mut out = Vec::new();
    let mut empty = 0;
    while out.len() < max && empty < EMPTY_POLLS {
        match consumer.poll(POLL_TIMEOUT) {
            Some(Ok(borrowed)) => {
                empty = 0;
                match serde_json::from_slice::<Message>(borrowed.payload().unwrap_or_default()) {
                    Ok(message) => out.push(message),
                    Err(error) => tracing::warn!(%error, topic, "Kafka 死信消息解析失败，跳过"),
                }
            }
            Some(Err(error)) => {
                tracing::warn!(%error, topic, "Kafka 死信窥视出错");
                empty += 1;
            }
            None => empty += 1,
        }
    }
    Ok(out)
}
