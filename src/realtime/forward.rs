//! 跨实例转发（`[realtime].forward_via_queue = true`）。
//!
//! ## 为什么不是「一个队列大家抢」
//!
//! 队列是**点对点**语义（Redis LIST / Kafka consumer group 都是「一条消息只投给
//! 一个消费者」）。如果所有实例消费同一个队列名，A 实例发的消息会被 A 自己抢走并
//! 丢弃（`origin` 过滤），B 永远收不到。
//!
//! 因此这里用 **每实例一个队列**：
//!
//! ```text
//! 注册表(cache) ── peers ──▶ 生产者把信封投到每个 peer 的队列
//! 本实例消费 realtime.forward.{self} ──▶ 本地广播
//! ```
//!
//! ## 组成
//!
//! - [`Registry`]：实例注册表，落在 `cache`（JSON map + TTL，心跳刷新）。**需要 `cache` feature**
//! - [`Envelope`]：带 `origin` 的信封，消费端丢弃自己发的，防回环
//! - [`spawn_consumer`]：消费本实例队列并灌回本地 `Hub`
//!
//! ## 已知限制
//!
//! - 注册表是「读-改-写」，并发注册可能短暂丢条目；心跳（5s）会自动补回
//! - 转发是**尽力而为**：投递失败只告警，不影响本地广播（`Hub::publish` 仍是同步的）
//! - 实例下线后，其队列里未被消费的消息会留在后端（按 `[queue].dlq` 策略处理）
//! - **队列后端是信任边界**：消费端把 `Envelope` 当「已授权发布」直接广播，能写这个
//!   队列的人等于能往任意频道发消息。必须内网、独立凭证、不与其它系统共用

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cache::CacheService;
use crate::errors::AppResult;
use crate::queue::QueueService;
use crate::realtime::hub::Hub;
use crate::realtime::message::RealtimeMessage;

/// 队列名前缀（实际名 = `{prefix}.{instance}`）。
pub const QUEUE_PREFIX: &str = "realtime.forward";
/// 注册表键（`SecureStore` 会加 `{namespace}:r:` 前缀）。
pub const REGISTRY_KEY: &str = "instances";
/// 注册心跳间隔。
const HEARTBEAT: Duration = Duration::from_secs(5);
/// 注册过期时长（心跳的 3 倍）。
const REGISTRY_TTL: Duration = Duration::from_secs(15);

/// 某实例的队列名。
#[must_use]
pub fn queue_name(instance: &str) -> String {
    format!("{QUEUE_PREFIX}.{instance}")
}

/// 转发信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// 发布方实例 id（消费端据此跳过自己的消息）。
    pub origin: String,
    /// 频道。
    pub topic: String,
    /// 消息体。
    pub message: RealtimeMessage,
}

/// 实例注册表（存于 `cache`）。
#[derive(Clone)]
pub struct Registry {
    cache: CacheService,
    instance: String,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// 以缓存与实例 id 构造。
    #[must_use]
    pub fn new(cache: CacheService, instance: impl Into<String>) -> Self {
        Self {
            cache,
            instance: instance.into(),
        }
    }

    /// 注册/续期本实例。
    ///
    /// # Errors
    /// 缓存读写失败时返回错误（调用方通常只告警）。
    pub async fn register(&self) -> AppResult<()> {
        let now = crate::utils::now_unix_secs();
        let mut map = self.load().await?;
        map.insert(self.instance.clone(), now + REGISTRY_TTL.as_secs() as i64);
        map.retain(|_, expires_at| *expires_at > now);
        self.store(&map).await
    }

    /// 当前存活的其他实例（已剔除过期与自身）。
    ///
    /// # Errors
    /// 缓存读失败时返回错误。
    pub async fn peers(&self) -> AppResult<Vec<String>> {
        let now = crate::utils::now_unix_secs();
        let map = self.load().await?;
        Ok(map
            .into_iter()
            .filter(|(id, expires_at)| id != &self.instance && *expires_at > now)
            .map(|(id, _)| id)
            .collect())
    }

    async fn load(&self) -> AppResult<HashMap<String, i64>> {
        match self.registry().get(REGISTRY_KEY).await? {
            Some(bytes) => match serde_json::from_slice(&bytes) {
                Ok(map) => Ok(map),
                Err(error) => {
                    // 解析失败说明键被别的东西写脏了（共库 / 手工改 / 版本不兼容）。
                    // 静默 `unwrap_or_default()` 会假装注册表是空的，把「所有 peer 都消失」
                    // 变成一件无声无息的事；这里至少留下一条可查的痕迹。
                    tracing::warn!(
                        %error,
                        bytes = bytes.len(),
                        "实时实例注册表解析失败，按空表处理（本实例会重新注册）"
                    );
                    Ok(HashMap::new())
                }
            },
            None => Ok(HashMap::new()),
        }
    }

    async fn store(&self, map: &HashMap<String, i64>) -> AppResult<()> {
        let bytes = serde_json::to_vec(map).unwrap_or_default();
        // TTL 覆盖两次心跳，避免实例静默下线后注册表永久残留
        self.registry()
            .put(REGISTRY_KEY, &bytes, REGISTRY_TTL * 4)
            .await
    }

    /// 注册表落在 `Class::Realtime`（`{ns}:r:`），与业务值键空间隔离。
    fn registry(&self) -> crate::cache::SecureStore {
        self.cache.secure(crate::cache::Class::Realtime)
    }

    /// 启动心跳任务（每 `HEARTBEAT` 续期一次）。
    ///
    /// 返回句柄由 [`Forwarder`] 持有，[`Hub::shutdown`] 时 abort——否则进程关闭后
    /// 任务仍会每 5s 往注册表写一次，把「已下线」的实例续期成「存活」。
    pub fn spawn_heartbeat(&self) -> Arc<tokio::task::JoinHandle<()>> {
        let registry = self.clone();
        Arc::new(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HEARTBEAT);
            loop {
                ticker.tick().await;
                if let Err(error) = registry.register().await {
                    tracing::warn!(%error, "实时实例注册心跳失败");
                }
            }
        }))
    }
}

/// 转发上下文（`Hub` 内部持有）。
#[derive(Clone)]
pub struct Forwarder {
    /// 队列服务（投递到 peer 队列 + 消费自己的队列）。
    pub queue: QueueService,
    /// 实例注册表。
    pub registry: Registry,
    /// 本实例 id。
    pub instance: Arc<str>,
    /// 注册表心跳任务（关闭时 abort）。
    pub heartbeat: Arc<tokio::task::JoinHandle<()>>,
}

impl Forwarder {
    /// 停止后台任务。
    pub fn stop(&self) {
        self.heartbeat.abort();
    }
    /// 投递到所有 peer 的队列（尽力而为）。
    pub async fn fanout(&self, topic: &str, message: RealtimeMessage) {
        let peers = match self.registry.peers().await {
            Ok(peers) => peers,
            Err(error) => {
                tracing::warn!(%error, "读取实时实例注册表失败，跳过跨实例转发");
                return;
            }
        };
        if peers.is_empty() {
            return;
        }

        let envelope = Envelope {
            origin: self.instance.to_string(),
            topic: topic.to_string(),
            message,
        };
        let Ok(payload) = serde_json::to_vec(&envelope) else {
            tracing::warn!(%topic, "转发消息序列化失败");
            return;
        };

        for peer in peers {
            let producer = self.queue.producer(queue_name(&peer));
            if let Err(error) = producer.publish(payload.clone()).await {
                tracing::warn!(%error, peer = %peer, "实时消息转发投递失败（不影响本地广播）");
            }
        }
    }
}

/// 消费本实例队列，把其他实例的消息灌回本地 `Hub`。
///
/// **信任边界**：队列里的 `Envelope` 会被当成「已授权发布」直接广播，因此队列后端
/// 必须是内网、独立凭证、不与其它系统共用——能往这个队列写的人等于能往任意频道发消息。
/// 即便如此，这里仍按入口同一套规则校验频道名与 payload 大小（纵深防御）。
pub fn spawn_consumer(hub: Hub, queue: QueueService) -> tokio::task::JoinHandle<()> {
    let instance = hub.instance_id().to_string();
    tokio::spawn(async move {
        let consumer = queue.consumer(queue_name(&instance));
        tracing::info!(queue = %queue_name(&instance), "实时转发消费者已启动");

        while !hub.is_closed() {
            match consumer.recv().await {
                Ok(Some(raw)) => {
                    match serde_json::from_slice::<Envelope>(&raw.payload) {
                        Ok(envelope) => {
                            // 防御：正常不会收到自己发的（只投给 peer），留一道保险
                            if envelope.origin != hub.instance_id()
                                && let Err(error) =
                                    hub.publish_local(&envelope.topic, envelope.message)
                            {
                                tracing::warn!(
                                    %error,
                                    topic = %crate::realtime::topic::log_view(&envelope.topic),
                                    "转发消息本地广播失败（频道名非法 / 超大小上限？），丢弃"
                                );
                            }
                        }
                        Err(error) => tracing::warn!(%error, "转发消息解析失败，丢弃"),
                    }
                    if let Err(error) = consumer.ack(&raw).await {
                        tracing::warn!(%error, "转发消息 ack 失败");
                    }
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(error) => {
                    tracing::warn!(%error, "实时转发消费失败，稍后重试");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        tracing::debug!("实时转发消费者已退出");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_per_instance_queue_name() {
        assert_eq!(queue_name("abc"), "realtime.forward.abc");
        assert_ne!(queue_name("a"), queue_name("b"));
    }

    #[test]
    fn should_roundtrip_envelope() {
        let envelope = Envelope {
            origin: "node-a".to_string(),
            topic: "room:1".to_string(),
            message: RealtimeMessage::text("hi"),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.origin, "node-a");
        assert_eq!(back.topic, "room:1");
        assert_eq!(back.message.payload, "hi");
    }
}
