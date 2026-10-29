//! 队列服务：按 `[queue].backend` 选后端，提供生产 / 消费 / 死信入口。

use std::sync::Arc;
use std::time::Duration;

use crate::config::QueueSection;
use crate::config::sections::queue::QueueBackendKind;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::queue::dlq;
use crate::queue::{Consumer, MemoryQueue, Message, Producer, QueueBackend};

#[allow(dead_code)]
fn missing(key: &str) -> AppError {
    AppError::biz(
        ErrorCode::SERVICE_UNAVAILABLE,
        format!("[queue].{key} 未配置"),
    )
}

/// 配置指定了后端，但未启用对应 feature（仅在该 feature 关闭时被调用）。
#[allow(dead_code)]
fn not_compiled(backend: &str, feature: &str) -> AppError {
    AppError::biz(
        ErrorCode::SERVICE_UNAVAILABLE,
        format!("[queue].backend = \"{backend}\" 需要启用 `{feature}` feature"),
    )
}

/// 消费幂等的两阶段标记：`recv` 写 `processing`，`ack` 改写 `done`。
///
/// 为什么不是「`recv` 写一个终态键」：那样一旦消费者在「已占位、未处理完」之间崩溃，
/// 重投的消息会命中去重键被**永久跳过**（静默丢消息）。用短租约的 `processing` 表达
/// 「有人正在处理」，租约到期即视为放弃，重投可以重新占位再处理一次。
#[cfg(feature = "cache")]
const DEDUP_PROCESSING: &[u8] = b"processing";
/// 处理完成的终态标记（保留 `[queue].idempotency.ttl`）。
#[cfg(feature = "cache")]
const DEDUP_DONE: &[u8] = b"done";
/// 去重占位结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "cache"), allow(dead_code))]
pub(crate) enum DedupClaim {
    /// 占位成功：本消费者可以执行业务。
    First,
    /// 已处理完成，或正被另一个消费者处理：跳过这条投递。
    Duplicate,
}

/// 队列服务（`AppState.queue`）。
///
/// 以 `Arc` 共享；handler 经 `State<AppState>`（或 `State<QueueService>`，需
/// `FromRef`）访问。
#[derive(Clone)]
pub struct QueueService {
    backend: Arc<dyn QueueBackend>,
    config: QueueSection,
    policy: dlq::DeadLetterPolicy,
    /// 消费幂等去重（需 `cache` feature；去重键默认落 `cache-redis`）。
    #[cfg(feature = "cache")]
    cache: Option<crate::cache::CacheService>,
}

impl std::fmt::Debug for QueueService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueService")
            .field("backend", &self.config.backend)
            .finish()
    }
}

impl QueueService {
    /// 由后端与配置装配。
    #[must_use]
    pub fn new(backend: Arc<dyn QueueBackend>, config: QueueSection) -> Self {
        Self {
            backend,
            policy: dlq::DeadLetterPolicy::from_config(&config),
            config,
            #[cfg(feature = "cache")]
            cache: None,
        }
    }

    /// 按 `[queue]` 配置装配（连接对应后端）。
    ///
    /// # Errors
    /// 后端不可达、缺少连接配置，或未启用对应 feature 时返回错误。
    pub async fn from_config(config: &QueueSection) -> AppResult<Self> {
        let backend: Arc<dyn QueueBackend> = match config.backend {
            QueueBackendKind::Memory => {
                Arc::new(MemoryQueue::new().with_max_depth(config.max_depth))
            }

            QueueBackendKind::Redis => {
                #[cfg(feature = "queue-redis")]
                {
                    let url = config
                        .redis
                        .url
                        .as_deref()
                        .ok_or_else(|| missing("redis.url"))?;
                    Arc::new(
                        crate::queue::redis::RedisQueue::connect(url)
                            .await?
                            .with_visibility_timeout(config.visibility_timeout),
                    )
                }
                #[cfg(not(feature = "queue-redis"))]
                {
                    return Err(not_compiled("redis", "queue-redis"));
                }
            }

            QueueBackendKind::Amqp => {
                #[cfg(feature = "queue-amqp")]
                {
                    let url = config
                        .amqp
                        .url
                        .as_deref()
                        .ok_or_else(|| missing("amqp.url"))?;
                    Arc::new(
                        crate::queue::amqp::AmqpQueue::connect(url)
                            .await?
                            .with_ack_timeout(config.visibility_timeout),
                    )
                }
                #[cfg(not(feature = "queue-amqp"))]
                {
                    return Err(not_compiled("amqp", "queue-amqp"));
                }
            }

            QueueBackendKind::Kafka => {
                #[cfg(feature = "queue-kafka")]
                {
                    Arc::new(crate::queue::kafka::KafkaQueue::connect(
                        &config.kafka.brokers,
                    )?)
                }
                #[cfg(not(feature = "queue-kafka"))]
                {
                    return Err(not_compiled("kafka", "queue-kafka"));
                }
            }
        };

        Ok(Self::new(backend, config.clone()))
    }

    /// 注入缓存以启用消费幂等去重（`[queue].idempotency.enabled = true`）。
    ///
    /// 无缓存时配置项被忽略（启动期会记 warn）。
    #[cfg(feature = "cache")]
    #[must_use]
    pub fn with_cache(mut self, cache: crate::cache::CacheService) -> Self {
        self.cache = Some(cache);
        self
    }

    /// 当前配置。
    #[must_use]
    pub fn config(&self) -> &QueueSection {
        &self.config
    }

    /// 生产句柄。
    #[must_use]
    pub fn producer(&self, queue: impl Into<String>) -> Producer {
        Producer::new(self.clone(), queue.into())
    }

    /// 消费句柄。
    #[must_use]
    pub fn consumer(&self, queue: impl Into<String>) -> Consumer {
        Consumer::new(self.clone(), queue.into())
    }

    /// 待消费消息数。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    pub async fn len(&self, queue: &str) -> AppResult<usize> {
        self.backend.len(queue).await
    }

    /// 死信列表（查询）。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    pub async fn dead_letters(&self, queue: &str) -> AppResult<Vec<Message>> {
        self.backend.dead_letters(queue).await
    }

    /// 重放死信（最多 `max` 条）。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    pub async fn replay_dead(&self, queue: &str, max: usize) -> AppResult<usize> {
        self.backend.replay_dead(queue, max).await
    }

    /// 连通性探测。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    pub async fn ping(&self) -> AppResult<()> {
        self.backend.ping().await
    }

    /// 内部：校验消息体大小（`[queue].max_payload_bytes`）。
    ///
    /// 入队与出队都要过：只拦入队挡不住「别的生产者 / 老版本 / 手工灌入」的消息，
    /// 而反序列化一条超大消息足以让消费端 OOM。
    pub(crate) fn check_payload_size(&self, len: usize) -> AppResult<()> {
        let max = self.config.max_payload_bytes;
        if len > max {
            return Err(AppError::biz(
                ErrorCode::PAYLOAD_TOO_LARGE,
                format!("队列消息体 {len} 字节超过上限 {max} 字节（[queue].max_payload_bytes）"),
            ));
        }
        Ok(())
    }

    /// 内部：后端引用（供 Producer / Consumer 使用）。
    pub(crate) fn backend(&self) -> &Arc<dyn QueueBackend> {
        &self.backend
    }

    /// 内部：重试 / 死信策略。
    pub(crate) fn policy(&self) -> dlq::DeadLetterPolicy {
        self.policy
    }

    /// 内部：幂等去重开关是否生效。
    pub(crate) fn dedup_enabled(&self) -> bool {
        #[cfg(feature = "cache")]
        {
            self.config.idempotency.enabled && self.cache.is_some()
        }
        #[cfg(not(feature = "cache"))]
        {
            false
        }
    }

    /// 内部：去重存储不可用时是否拒绝消费（默认 `false`：降级为不去重）。
    pub(crate) fn dedup_fail_closed(&self) -> bool {
        #[cfg(feature = "cache")]
        {
            self.config.idempotency.fail_closed
        }
        #[cfg(not(feature = "cache"))]
        {
            false
        }
    }

    /// 内部：**原子占位**——首次投递返回 [`DedupClaim::First`]，重复投递返回
    /// [`DedupClaim::Duplicate`]。
    ///
    /// 用 `put_once`（`SET NX`）而不是「先查后写」：后者存在窗口，两个消费者会
    /// 同时判定「没消费过」然后各处理一次。
    ///
    /// 写入的是 `processing` + **短租约**（`[queue].idempotency.lease`）：
    /// - 消费者崩溃 → 租约到期 → 重投可以重新占位（不会永久跳过）；
    /// - 处理完成 → [`dedup_complete`](Self::dedup_complete) 改写为 `done` + 长保留。
    ///
    /// # Errors
    /// 去重存储不可用时返回 `50004`。
    pub(crate) async fn dedup_claim(&self, queue: &str, id: &str) -> AppResult<DedupClaim> {
        #[cfg(feature = "cache")]
        if let Some(cache) = &self.cache {
            let store = cache.secure(crate::cache::Class::Queue);
            let key = dedup_key(queue, id);
            let lease = self.config.idempotency.lease;

            if store.put_once(&key, DEDUP_PROCESSING, lease).await? {
                return Ok(DedupClaim::First);
            }

            // 占位失败：要么已 done，要么另一个消费者正在处理（processing 仍在租约内）。
            return Ok(match store.get(&key).await? {
                Some(value) if value.as_slice() == DEDUP_DONE => DedupClaim::Duplicate,
                Some(_) => DedupClaim::Duplicate,
                // 租约恰好在「占位失败」与「读取」之间到期：再抢一次，抢不到才算重复。
                None => {
                    if store.put_once(&key, DEDUP_PROCESSING, lease).await? {
                        DedupClaim::First
                    } else {
                        DedupClaim::Duplicate
                    }
                }
            });
        }
        let _ = (queue, id);
        Ok(DedupClaim::First)
    }

    /// 内部：把占位从 `processing` 提升为 `done`（`ack` 成功后调用）。
    ///
    /// 失败只告警、不改变 `ack` 的结果：最坏情况下这条消息在租约过期后被**再处理一次**
    /// （at-least-once），比「已确认但没标记、被当成新消息重放」更容易接受。
    pub(crate) async fn dedup_complete(&self, queue: &str, id: &str) {
        #[cfg(feature = "cache")]
        if self.config.idempotency.enabled
            && let Some(cache) = &self.cache
        {
            let store = cache.secure(crate::cache::Class::Queue);
            let ttl = self.config.idempotency.ttl;
            if let Err(error) = store.put(&dedup_key(queue, id), DEDUP_DONE, ttl).await {
                tracing::warn!(
                    %error,
                    queue,
                    id,
                    "标记消费完成失败：租约到期后该消息可能被重复消费一次"
                );
            }
        }
        let _ = (queue, id);
    }

    /// 内部：释放占位（消费失败、消息要重投时必须调用，否则重投会被当成重复消息）。
    ///
    /// 只删 `processing`（`del_if_eq`）：`done` 是终态，误调 `nack` 不应该把已完成的
    /// 消息重新变成可执行。
    pub(crate) async fn dedup_release(&self, queue: &str, id: &str) {
        #[cfg(feature = "cache")]
        if self.config.idempotency.enabled
            && let Some(cache) = &self.cache
        {
            let store = cache.secure(crate::cache::Class::Queue);
            if let Err(error) = store
                .del_if_eq(&dedup_key(queue, id), DEDUP_PROCESSING)
                .await
            {
                tracing::warn!(%error, "释放消费去重占位失败（可能导致该消息被跳过）");
            }
        }
        let _ = (queue, id);
    }
}

/// 去重键。
///
/// 队列名经 [`validate_queue_name`] 校验（不含 `:`），`message.id` 由框架生成
/// （UUID），因此 `{queue}:{id}` 不会跨队列碰撞。
#[cfg(feature = "cache")]
fn dedup_key(queue: &str, id: &str) -> String {
    format!("{queue}:{id}")
}

/// 队列名长度上限。
pub const QUEUE_NAME_MAX_LEN: usize = 64;

/// 校验队列名（`[A-Za-z0-9_.-]{1,64}`，且不能以 `.` 开头）。
///
/// 队列名会直接进入 Redis 键（`aurora:queue:{queue}:ready`）、AMQP 的
/// `queue_declare` 与 Kafka 的 `subscribe`。若把请求参数原样当队列名，攻击者可以：
///
/// - 在 broker 上无限创建 durable 队列 / topic（持久化资源耗尽）；
/// - 让 Kafka 为每个新名字建一个消费者（线程 + fd 泄漏）；
/// - 订阅到不属于本应用的 topic（越权读别人的消息）。
///
/// 因此框架在 `producer` / `consumer` 入口统一校验；业务侧若需要「队列名来自配置」，
/// 也可以直接调用本函数做白名单外的兜底。
///
/// # Errors
/// 名称为空、超长、含非法字符，或以 `.` 开头时返回 `10001`。
pub fn validate_queue_name(queue: &str) -> AppResult<()> {
    if queue.is_empty() {
        return Err(AppError::invalid_param("队列名不能为空"));
    }
    if queue.len() > QUEUE_NAME_MAX_LEN {
        return Err(AppError::invalid_param(format!(
            "队列名不能超过 {QUEUE_NAME_MAX_LEN} 字节"
        )));
    }
    if queue.starts_with('.') {
        return Err(AppError::invalid_param(
            "队列名不能以 `.` 开头（AMQP / Kafka 里 `.` 前缀保留给内部 topic）",
        ));
    }
    if let Some(bad) = queue
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
    {
        return Err(AppError::invalid_param(format!(
            "队列名只允许 [A-Za-z0-9_.-]，发现非法字符 {bad:?}"
        )));
    }
    Ok(())
}

pub(crate) const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_generate_unique_message_ids() {
        let a = Message::new("q", b"1".to_vec());
        let b = Message::new("q", b"2".to_vec());
        assert_ne!(a.id, b.id);
        assert_eq!(a.attempts, 0);
        assert_eq!(a.queue, "q");
    }

    #[tokio::test]
    async fn should_reject_redis_backend_without_url() {
        let config = QueueSection {
            backend: QueueBackendKind::Redis,
            ..QueueSection::default()
        };
        let result = QueueService::from_config(&config).await;
        assert!(
            result.is_err(),
            "缺 redis.url 应报错（未启用 feature 时同理）"
        );
    }

    #[tokio::test]
    async fn should_build_memory_backend_by_default() {
        let service = QueueService::from_config(&QueueSection::default())
            .await
            .unwrap();
        assert_eq!(service.config().backend, QueueBackendKind::Memory);
    }

    #[test]
    fn should_reject_illegal_queue_names() {
        assert!(validate_queue_name("orders").is_ok());
        assert!(validate_queue_name("orders.v2_2026-01").is_ok());

        assert!(validate_queue_name("").is_err(), "空名应拒绝");
        assert!(
            validate_queue_name("orders:ready").is_err(),
            "冒号会污染 Redis 键结构"
        );
        assert!(
            validate_queue_name("../orders").is_err(),
            "`.` 开头保留给内部 topic"
        );
        assert!(validate_queue_name("orders\n").is_err(), "控制字符应拒绝");
        assert!(
            validate_queue_name(&"a".repeat(QUEUE_NAME_MAX_LEN + 1)).is_err(),
            "超长应拒绝"
        );
        assert!(
            validate_queue_name("orders/{id}").is_err(),
            "斜杠等字符不应放行"
        );
    }

    /// 构造一个启用幂等去重的服务（`cache` feature 下才有去重）。
    #[cfg(feature = "cache")]
    async fn dedup_service(lease: Duration) -> QueueService {
        let cache =
            crate::cache::CacheService::from_config(&crate::config::CacheSection::default())
                .await
                .unwrap();
        let mut config = QueueSection::default();
        config.idempotency.enabled = true;
        config.idempotency.lease = lease;
        QueueService::from_config(&config)
            .await
            .unwrap()
            .with_cache(cache)
    }

    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn should_mark_done_only_after_ack() {
        let service = dedup_service(Duration::from_secs(60)).await;

        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::First,
            "首次应占位成功"
        );
        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::Duplicate,
            "处理中不应被第二个消费者重复占位"
        );

        service.dedup_complete("orders", "m1").await;
        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::Duplicate,
            "已完成的 id 应一直判为重复"
        );
    }

    /// 回归：旧实现「占位即终态」，消费者崩溃后重投的消息会被永久跳过。
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn should_reclaim_after_lease_expires() {
        let service = dedup_service(Duration::from_millis(30)).await;

        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::First
        );
        // 模拟「占位后进程崩溃」：不 ack、不 release，只等租约过期。
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::First,
            "租约过期后重投必须能重新占位，否则消息被永久跳过"
        );
    }

    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn should_release_claim_on_nack() {
        let service = dedup_service(Duration::from_secs(60)).await;

        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::First
        );
        service.dedup_release("orders", "m1").await;
        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::First,
            "nack 释放占位后，重投应能再次占位"
        );
    }

    /// `done` 是终态：误调 `nack` 不应该把已完成的 id 变回可执行。
    #[cfg(feature = "cache")]
    #[tokio::test]
    async fn should_not_release_done_marker() {
        let service = dedup_service(Duration::from_secs(60)).await;

        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::First
        );
        service.dedup_complete("orders", "m1").await;
        service.dedup_release("orders", "m1").await;

        assert_eq!(
            service.dedup_claim("orders", "m1").await.unwrap(),
            DedupClaim::Duplicate
        );
    }
}
