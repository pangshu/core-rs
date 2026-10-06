//! 连接注册表与频道广播（文档 三·15）。
//!
//! - 单机：[`Hub::broadcast`] 直投频道内全部连接；
//! - 多实例（v1 经 **Redis Pub/Sub**，见 [`super::forward`]）：`[realtime].forward =
//!   "queue"` 且 cache 后端为 redis 时，`broadcast` 把消息发布到转发 channel，
//!   各实例（含本机）的订阅任务在本机重放——单一路径，天然不重复投递。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

use crate::config::sections::RealtimeSettings;

use super::RealtimeMessage;

/// 每个连接持有的发送端（hub → 连接方向）。
/// **有界**通道：打满即由 [`Hub::broadcast_local`] 踢除慢消费者——
/// unbounded 通道在客户端挂起不读时会无界堆积直到 OOM。
pub type ConnectionSender = mpsc::Sender<RealtimeMessage>;

/// 连接注册表：topic → 连接集合（conn_id → sender）
pub struct Hub {
    max_connections: usize,
    send_buffer: usize,
    heartbeat_secs: u64,
    connections: AtomicUsize,
    next_conn_id: AtomicUsize,
    channels: std::sync::Mutex<HashMap<String, HashMap<usize, ConnectionSender>>>,
    forward_topic: String,
    /// 跨实例转发器（cache-redis feature；App/CoreState 装配时按配置注入）
    #[cfg(feature = "cache-redis")]
    forwarder: std::sync::OnceLock<std::sync::Arc<super::forward::Forwarder>>,
}

impl Hub {
    pub fn new(settings: &RealtimeSettings) -> Self {
        Self {
            max_connections: settings.max_connections.max(1),
            send_buffer: settings.send_buffer.clamp(1, 65536),
            heartbeat_secs: settings.heartbeat_secs.max(5),
            connections: AtomicUsize::new(0),
            next_conn_id: AtomicUsize::new(1),
            channels: std::sync::Mutex::new(HashMap::new()),
            forward_topic: settings.forward_topic.clone(),
            #[cfg(feature = "cache-redis")]
            forwarder: std::sync::OnceLock::new(),
        }
    }

    /// 心跳间隔（秒）
    pub fn heartbeat_secs(&self) -> u64 {
        self.heartbeat_secs
    }

    /// 连接切换频道（不重建 channel：sender 在注册表内移动）
    pub fn switch(&self, old_topic: &str, new_topic: &str, conn_id: usize) -> bool {
        if old_topic == new_topic {
            return true;
        }
        let mut map = Self::lock(&self.channels);
        let tx = map
            .get_mut(old_topic)
            .and_then(|conns| conns.remove(&conn_id));
        let Some(tx) = tx else { return false };
        if map.get(old_topic).map(|c| c.is_empty()).unwrap_or(false) {
            map.remove(old_topic);
        }
        map.entry(new_topic.to_string())
            .or_default()
            .insert(conn_id, tx);
        true
    }

    fn lock(
        m: &std::sync::Mutex<HashMap<String, HashMap<usize, ConnectionSender>>>,
    ) -> std::sync::MutexGuard<'_, HashMap<String, HashMap<usize, ConnectionSender>>> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 注册连接到频道（断线由调用方触发 unregister 清理）。返回 conn_id 与
    /// hub→连接 的接收端。
    pub fn register(
        &self,
        topic: &str,
    ) -> Result<(usize, mpsc::Receiver<RealtimeMessage>), RegisterError> {
        // CAS 占位：先 load 再无条件 fetch_add 的两步之间，并发可全部通过检查，
        // 上限会失效；compare_exchange_weak 循环保证原子性（1.85 MSRV 可用）
        loop {
            let current = self.connections.load(Ordering::SeqCst);
            if current >= self.max_connections {
                return Err(RegisterError(self.max_connections));
            }
            match self.connections.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(_) => continue, // 其他连接并发注册：重读后重试
            }
        }
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(self.send_buffer);
        Self::lock(&self.channels)
            .entry(topic.to_string())
            .or_default()
            .insert(conn_id, tx);
        Ok((conn_id, rx))
    }

    /// 注销连接（断线清理；幂等）
    pub fn unregister(&self, topic: &str, conn_id: usize) {
        let mut removed = false;
        {
            let mut map = Self::lock(&self.channels);
            if let Some(conns) = map.get_mut(topic) {
                if conns.remove(&conn_id).is_some() {
                    removed = true;
                }
                if conns.is_empty() {
                    map.remove(topic);
                }
            }
        }
        if removed {
            self.connections.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// 向频道广播。转发未启用 → 本机直投；启用（Redis Pub/Sub）→ 发布到转发
    /// channel，由各实例（含本机）的订阅任务重放——单一路径不重复投递；
    /// 发布失败降级本机直投并告警。返回**本机直接送达**的连接数
    /// （转发启用时本机送达走重放路径，通常为 0）。
    pub async fn broadcast(&self, msg: &RealtimeMessage) -> usize {
        #[cfg(feature = "cache-redis")]
        if let Some(forwarder) = self.forwarder.get() {
            if let Err(e) = forwarder.publish(msg).await {
                tracing::warn!(error = %e, "realtime forward publish failed, fallback to local broadcast");
                return self.broadcast_local(msg);
            }
            return 0;
        }
        self.broadcast_local(msg)
    }

    /// 本机直投（转发订阅任务的重放入口；不含跨实例发布）。
    /// 有界缓冲打满即踢除慢消费者（宁可丢客户端、不无界堆积 OOM）；
    /// 已关闭的 sender 同步剔除，避免幽灵连接占满 max_connections。
    pub(crate) fn broadcast_local(&self, msg: &RealtimeMessage) -> usize {
        let mut sent = 0;
        let mut kicked: Vec<usize> = Vec::new();
        {
            let mut map = Self::lock(&self.channels);
            if let Some(conns) = map.get_mut(&msg.topic) {
                for (conn_id, tx) in conns.iter() {
                    match tx.try_send(msg.clone()) {
                        Ok(()) => sent += 1,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                conn_id,
                                topic = %msg.topic,
                                "slow consumer kicked (realtime send buffer full)"
                            );
                            kicked.push(*conn_id);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            kicked.push(*conn_id);
                        }
                    }
                }
                for id in &kicked {
                    conns.remove(id);
                }
                if conns.is_empty() {
                    map.remove(&msg.topic);
                }
            }
        }
        if !kicked.is_empty() {
            self.connections.fetch_sub(kicked.len(), Ordering::SeqCst);
        }
        sent
    }

    pub fn forward_topic(&self) -> &str {
        &self.forward_topic
    }

    /// 当前在线连接数
    pub fn online(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// 频道在线连接数
    pub fn online_in(&self, topic: &str) -> usize {
        Self::lock(&self.channels)
            .get(topic)
            .map(|c| c.len())
            .unwrap_or(0)
    }
}

#[cfg(feature = "cache-redis")]
impl Hub {
    /// 注入跨实例转发器（CoreState 装配时调用一次）
    pub fn set_forwarder(&self, forwarder: std::sync::Arc<super::forward::Forwarder>) {
        let _ = self.forwarder.set(forwarder);
    }
}

/// 注册失败：容量满
#[derive(Debug, thiserror::Error)]
#[error("realtime hub at capacity ({0} connections)")]
pub struct RegisterError(pub usize);

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> RealtimeSettings {
        RealtimeSettings::default()
    }

    #[tokio::test]
    async fn register_broadcast_unregister() {
        let hub = Hub::new(&settings());
        let (_id1, mut rx1) = hub.register("user:1").unwrap();
        let (_id2, mut rx2) = hub.register("user:1").unwrap();
        assert_eq!(hub.online_in("user:1"), 2);

        let msg = RealtimeMessage::new("notify", "user:1", serde_json::json!({"a": 1}));
        assert_eq!(hub.broadcast(&msg).await, 2);
        assert_eq!(rx1.recv().await.unwrap().event, "notify");
        assert_eq!(rx2.recv().await.unwrap().event, "notify");

        hub.unregister("user:1", _id1);
        assert_eq!(hub.online_in("user:1"), 1);
    }
}

#[cfg(test)]
mod slow_consumer_tests {
    use super::*;
    use futures::StreamExt as _;

    fn settings() -> RealtimeSettings {
        RealtimeSettings {
            send_buffer: 2,
            max_connections: 4,
            ..Default::default()
        }
    }

    fn msg(topic: &str) -> RealtimeMessage {
        RealtimeMessage::new("notify", topic, serde_json::json!({"a": 1}))
    }

    /// 慢消费者（注册后不读）打满有界缓冲后被剔除，而不是无界堆积 OOM（P0-10）
    #[tokio::test]
    async fn slow_consumer_is_kicked_when_buffer_full() {
        let hub = Hub::new(&settings());
        let (_id, mut rx) = hub.register("t").unwrap(); // 持有 rx 但不读

        let m = msg("t");
        for _ in 0..10 {
            hub.broadcast(&m).await;
        }

        assert_eq!(hub.online(), 0, "慢消费者必须被剔除");
        assert_eq!(hub.online_in("t"), 0);
        // sender 已关闭：接收端收尾
        assert!(rx.recv().await.is_none() || rx.is_closed());
    }

    /// max_connections 上限在并发注册下依然成立（P1-54 CAS）
    #[tokio::test]
    async fn max_connections_holds_under_concurrency() {
        let hub = Hub::new(&settings()); // max_connections = 4
        let results = futures::stream::iter(0..16)
            .then(|_| {
                let hub = &hub;
                async move { hub.register("t").is_ok() }
            })
            .collect::<Vec<_>>()
            .await;
        let ok = results.into_iter().filter(|r| *r).count();
        assert_eq!(ok, 4, "并发注册不得超过 max_connections");
        assert_eq!(hub.online(), 4);
    }
}
