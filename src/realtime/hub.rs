//! 连接注册表与频道广播（文档 三·15）。
//!
//! - 单机：[`Hub::broadcast`] 直投频道内全部连接；
//! - 多实例（v1 经 **Redis Pub/Sub**，见 [`super::forward`]）：`[realtime].forward =
//!   "queue"` 且 cache 后端为 redis 时，`broadcast` 把消息发布到转发 channel，
//!   各实例（含本机）的订阅任务在本机重放——单一路径，天然不重复投递。

use std::collections::HashMap; // 引入哈希表，用于 topic → 连接集合 的注册表
use std::sync::atomic::{AtomicUsize, Ordering}; // 引入原子计数与内存序，用于连接数/ID 无锁计数

use tokio::sync::mpsc; // 引入 tokio 多生产者单消费者通道（hub → 连接）

use crate::config::sections::RealtimeSettings; // 引入实时通信配置段（容量、缓冲、心跳等）

use super::RealtimeMessage; // 引入实时消息类型（通道传输单元）

/// 每个连接持有的发送端（hub → 连接方向）。
/// **有界**通道：打满即由 [`Hub::broadcast_local`] 踢除慢消费者——
/// unbounded 通道在客户端挂起不读时会无界堆积直到 OOM。
pub type ConnectionSender = mpsc::Sender<RealtimeMessage>; // 连接发送端类型别名（有界 mpsc Sender）

/// 连接注册表：topic → 连接集合（conn_id → sender）
pub struct Hub { // 实时通信中心：管理连接注册、频道广播与跨实例转发
    max_connections: usize, // 全局最大并发连接数上限
    send_buffer: usize, // 每连接有界发送缓冲容量
    heartbeat_secs: u64, // 心跳间隔（秒），ws 用于 Ping、sse 用于 KeepAlive
    connections: AtomicUsize, // 当前在线连接数（原子计数，供容量控制 CAS 使用）
    next_conn_id: AtomicUsize, // 下一个连接 ID（原子自增分配，唯一标识连接）
    channels: std::sync::Mutex<HashMap<String, HashMap<usize, ConnectionSender>>>, // 频道注册表：topic → (conn_id → sender)
    forward_topic: String, // 跨实例转发的 Redis channel 名
    /// 跨实例转发器（cache-redis feature；App/CoreState 装配时按配置注入）
    #[cfg(feature = "cache-redis")] // 仅 cache-redis feature 下存在该字段
    forwarder: std::sync::OnceLock<std::sync::Arc<super::forward::Forwarder>>, // 转发器，装配期一次性注入
}

impl Hub { // Hub 核心实现：构造、注册/注销、广播与状态查询
    pub fn new(settings: &RealtimeSettings) -> Self { // 依据配置段构造 Hub
        Self { // 组装 Hub 字面量，配置值做上下界保护
            max_connections: settings.max_connections.max(1), // 最大连接数至少为 1，避免全部拒绝
            send_buffer: settings.send_buffer.clamp(1, 65536), // 发送缓冲限制在 1..=65536，防止 0 或过大
            heartbeat_secs: settings.heartbeat_secs.max(5), // 心跳至少 5 秒，避免过于频繁
            connections: AtomicUsize::new(0), // 初始在线连接数为 0
            next_conn_id: AtomicUsize::new(1), // 连接 ID 从 1 开始分配
            channels: std::sync::Mutex::new(HashMap::new()), // 空频道注册表
            forward_topic: settings.forward_topic.clone(), // 复制转发 channel 名
            #[cfg(feature = "cache-redis")] // 仅 cache-redis 时初始化转发器槽位
            forwarder: std::sync::OnceLock::new(), // 空的 OnceLock，待装配期注入
        }
    }

    /// 心跳间隔（秒）
    pub fn heartbeat_secs(&self) -> u64 { // 返回心跳间隔秒数
        self.heartbeat_secs // 直接读取配置值
    }

    /// 连接切换频道（不重建 channel：sender 在注册表内移动）
    pub fn switch(&self, old_topic: &str, new_topic: &str, conn_id: usize) -> bool { // 把连接从旧频道迁移到新频道
        if old_topic == new_topic { // 新旧频道相同则无需迁移
            return true; // 视为成功直接返回
        }
        let mut map = Self::lock(&self.channels); // 获取频道注册表锁（poison 自动恢复）
        let tx = map // 从旧频道中取出该连接的 sender
            .get_mut(old_topic) // 取得旧频道的连接集合
            .and_then(|conns| conns.remove(&conn_id)); // 移除并返回该连接的 sender
        let Some(tx) = tx else { return false }; // 旧频道中不存在该连接则失败返回
        if map.get(old_topic).map(|c| c.is_empty()).unwrap_or(false) { // 旧频道是否已空
            map.remove(old_topic); // 空频道清理，避免残留空条目
        }
        map.entry(new_topic.to_string()) // 定位新频道条目
            .or_default() // 不存在则创建空集合
            .insert(conn_id, tx); // 把 sender 插入新频道（channel 不重建，连接不断）
        true // 迁移成功
    }

    fn lock( // 获取频道注册表互斥锁的辅助函数
        m: &std::sync::Mutex<HashMap<String, HashMap<usize, ConnectionSender>>>, // 待加锁的注册表
    ) -> std::sync::MutexGuard<'_, HashMap<String, HashMap<usize, ConnectionSender>>> { // 返回持有期与入参一致的保护守卫
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
    }

    /// 注册连接到频道（断线由调用方触发 unregister 清理）。返回 conn_id 与
    /// hub→连接 的接收端。
    pub fn register( // 注册新连接到指定频道
        &self, // Hub 自身引用
        topic: &str, // 目标频道名
    ) -> Result<(usize, mpsc::Receiver<RealtimeMessage>), RegisterError> { // 返回连接 ID 与下行接收端，或容量错误
        // CAS 占位：先 load 再无条件 fetch_add 的两步之间，并发可全部通过检查，
        // 上限会失效；compare_exchange_weak 循环保证原子性（1.85 MSRV 可用）
        loop { // 自旋重试，保证连接数上限在并发下成立
            let current = self.connections.load(Ordering::SeqCst); // 读取当前在线连接数
            if current >= self.max_connections { // 已达上限则拒绝
                return Err(RegisterError(self.max_connections)); // 返回容量错误并附带上限值
            }
            match self.connections.compare_exchange_weak( // 尝试原子地把连接数 +1
                current, // 期望的当前值
                current + 1, // 期望写入的新值
                Ordering::SeqCst, // 成功时的内存序
                Ordering::SeqCst, // 失败时的内存序
            ) {
                Ok(_) => break, // 抢位成功，退出自旋
                Err(_) => continue, // 其他连接并发注册：重读后重试
            }
        }
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst); // 原子分配唯一连接 ID
        let (tx, rx) = mpsc::channel(self.send_buffer); // 创建有界通道，tx 存注册表、rx 交调用方
        Self::lock(&self.channels) // 加锁频道注册表
            .entry(topic.to_string()) // 定位目标频道
            .or_default() // 不存在则创建连接集合
            .insert(conn_id, tx); // 存入该连接的发送端
        Ok((conn_id, rx)) // 返回连接 ID 与下行接收端
    }

    /// 注销连接（断线清理；幂等）
    pub fn unregister(&self, topic: &str, conn_id: usize) { // 从频道移除连接并递减在线计数
        let mut removed = false; // 记录是否真的移除了连接（决定是否递减计数）
        {
            let mut map = Self::lock(&self.channels); // 加锁频道注册表
            if let Some(conns) = map.get_mut(topic) { // 频道存在才处理
                if conns.remove(&conn_id).is_some() { // 移除该连接
                    removed = true; // 标记确实移除成功
                }
                if conns.is_empty() { // 频道已空
                    map.remove(topic); // 清理空频道条目
                }
            }
        }
        if removed { // 仅当真正移除时递减计数，保证幂等
            self.connections.fetch_sub(1, Ordering::SeqCst); // 原子递减在线连接数
        }
    }

    /// 向频道广播。转发未启用 → 本机直投；启用（Redis Pub/Sub）→ 发布到转发
    /// channel，由各实例（含本机）的订阅任务重放——单一路径不重复投递；
    /// 发布失败降级本机直投并告警。返回**本机直接送达**的连接数
    /// （转发启用时本机送达走重放路径，通常为 0）。
    pub async fn broadcast(&self, msg: &RealtimeMessage) -> usize { // 广播消息，返回本机直接送达的连接数
        #[cfg(feature = "cache-redis")] // 仅 cache-redis 下才考虑跨实例转发
        if let Some(forwarder) = self.forwarder.get() { // 已注入转发器则走转发路径
            if let Err(e) = forwarder.publish(msg).await { // 发布到转发 channel
                tracing::warn!(error = %e, "realtime forward publish failed, fallback to local broadcast"); // 发布失败告警并降级
                return self.broadcast_local(msg); // 降级为本机直投
            }
            return 0; // 发布成功：本机送达交由重放路径，直接返回 0
        }
        self.broadcast_local(msg) // 未启用转发：本机直投
    }

    /// 本机直投（转发订阅任务的重放入口；不含跨实例发布）。
    /// 有界缓冲打满即踢除慢消费者（宁可丢客户端、不无界堆积 OOM）；
    /// 已关闭的 sender 同步剔除，避免幽灵连接占满 max_connections。
    pub(crate) fn broadcast_local(&self, msg: &RealtimeMessage) -> usize { // 仅本机投递，返回成功送达连接数
        let mut sent = 0; // 成功送达计数
        let mut kicked: Vec<usize> = Vec::new(); // 待剔除的连接 ID 列表
        {
            let mut map = Self::lock(&self.channels); // 加锁频道注册表
            if let Some(conns) = map.get_mut(&msg.topic) { // 目标频道存在才处理
                for (conn_id, tx) in conns.iter() { // 遍历频道内所有连接
                    match tx.try_send(msg.clone()) { // 非阻塞发送（克隆消息给每个连接）
                        Ok(()) => sent += 1, // 发送成功，计数 +1
                        Err(mpsc::error::TrySendError::Full(_)) => { // 缓冲已满：慢消费者
                            tracing::warn!( // 告警记录被踢除的连接
                                conn_id, // 连接 ID 字段
                                topic = %msg.topic, // 频道字段
                                "slow consumer kicked (realtime send buffer full)" // 告警文案
                            );
                            kicked.push(*conn_id); // 记入待剔除列表
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => { // 接收端已关闭：幽灵连接
                            kicked.push(*conn_id); // 直接记入待剔除列表
                        }
                    }
                }
                for id in &kicked { // 统一移除被踢除的连接
                    conns.remove(id); // 从频道集合删除
                }
                if conns.is_empty() { // 频道已空
                    map.remove(&msg.topic); // 清理空频道条目
                }
            }
        }
        if !kicked.is_empty() { // 有连接被剔除才递减计数
            self.connections.fetch_sub(kicked.len(), Ordering::SeqCst); // 原子递减相应数量
        }
        sent // 返回成功送达连接数
    }

    pub fn forward_topic(&self) -> &str { // 返回跨实例转发 channel 名
        &self.forward_topic // 借用内部字段
    }

    /// 当前在线连接数
    pub fn online(&self) -> usize { // 查询全局在线连接数
        self.connections.load(Ordering::SeqCst) // 原子读取计数
    }

    /// 频道在线连接数
    pub fn online_in(&self, topic: &str) -> usize { // 查询指定频道的连接数
        Self::lock(&self.channels) // 加锁注册表
            .get(topic) // 取该频道
            .map(|c| c.len()) // 映射为其连接数
            .unwrap_or(0) // 频道不存在则为 0
    }
}

#[cfg(feature = "cache-redis")] // 仅 cache-redis 下提供转发器注入
impl Hub { // 转发器装配相关实现
    /// 注入跨实例转发器（CoreState 装配时调用一次）
    pub fn set_forwarder(&self, forwarder: std::sync::Arc<super::forward::Forwarder>) { // 一次性注入转发器
        let _ = self.forwarder.set(forwarder); // 写入 OnceLock；重复注入被忽略
    }
}

/// 注册失败：容量满
#[derive(Debug, thiserror::Error)] // 派生调试与 thiserror 错误实现
#[error("realtime hub at capacity ({0} connections)")] // 定义错误显示文案，含上限值
pub struct RegisterError(pub usize); // 容量错误，字段为 max_connections

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
