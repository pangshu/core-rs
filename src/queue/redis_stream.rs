//! Redis Streams 消费组队列：at-least-once 投递。
//!
//! - 发布：`XADD`（每个 value 字段 JSON 序列化；空载荷用哨兵字段占位）；
//! - 订阅：`XGROUP CREATE ... MKSTREAM`（订阅即建组，订阅与启动之间发布的消息不丢）；
//! - 消费：`XREADGROUP GROUP g c COUNT n BLOCK t STREAMS k1 k2 >`（同组多实例
//!   负载均衡分摊，异组各收一份）；
//! - 确认：handler 成功 `XACK`；失败不 ACK，pending 里的消息闲置超过
//!   `claim_min_idle_secs` 被任意实例 `XCLAIM` 接手重投（消费者宕机自愈）；
//! - 死信：投递次数达到 `max_attempts` 的消息不再自动接管，留在 pending
//!   列表（XPENDING 可观测），人工处理后 XACK/XDEL。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use redis::aio::ConnectionManager;
use redis::streams::{StreamClaimReply, StreamReadOptions, StreamReadReply};
use redis::AsyncCommands;

use super::{Handler, Message, Queue, QueueError};
use crate::config::QueueRedisConfig;

/// 空载荷哨兵字段（XADD 拒绝全空 entry）
const EMPTY_SENTINEL: &str = "core-rs:empty";

#[derive(Default)]
struct State {
    handlers: HashMap<String, Handler>,
    /// 已订阅（已建组）的 stream 键，start 时确定读循环的读取集合
    topics: Vec<String>,
    started: bool,
    closed: bool,
    /// publish 时已确认存在消费组的 topic（省去每次 XINFO 往返）
    known_groups: HashSet<String>,
}

pub struct RedisQueue {
    conn: ConnectionManager,
    group: String,
    consumer: String,
    key_prefix: String,
    max_attempts: u64,
    block_secs: u64,
    claim_min_idle_secs: u64,
    batch: usize,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
}

impl RedisQueue {
    /// 建连即验证（坏地址在装配期报错，而非首条消息时）
    pub async fn new(url: &str, cfg: &QueueRedisConfig) -> Result<Self, QueueError> {
        let client = redis::Client::open(url)?;
        // ConnectionManager：断线自动重连
        let conn = ConnectionManager::new(client).await?;
        let consumer = if cfg.consumer.is_empty() {
            format!(
                "{}-{}-{:x}",
                std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string()),
                std::process::id(),
                rand_suffix()
            )
        } else {
            cfg.consumer.clone()
        };
        Ok(Self {
            conn,
            group: if cfg.group.is_empty() {
                "core-rs".to_string()
            } else {
                cfg.group.clone()
            },
            consumer,
            key_prefix: cfg.key_prefix.clone(),
            max_attempts: cfg.max_attempts.max(1),
            block_secs: cfg.block_secs.max(1),
            claim_min_idle_secs: cfg.claim_min_idle_secs.max(1),
            batch: cfg.batch.clamp(1, 1000),
            state: Arc::new(Mutex::new(State::default())),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn stream_key(&self, topic: &str) -> String {
        format!("{}{}", self.key_prefix, topic)
    }

    fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
        state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 发布前的消费组存在性检查（publish 到无消费组的 topic 必须显式报错）
    async fn ensure_group_for_publish(&self, topic: &str, key: &str) -> Result<(), QueueError> {
        if Self::lock(&self.state).known_groups.contains(topic) {
            return Ok(());
        }
        let mut conn = self.conn.clone();
        // XINFO GROUPS（RESP2）：嵌套数组，每组一段扁平 kv，如
        // [[name, g1, consumers, 1, pending, 0, last-delivered-id, ...]]
        let groups: Vec<Vec<String>> = redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|_| QueueError::NoHandler(topic.to_string()))?;
        let found = groups.iter().any(|g| {
            g.windows(2)
                .any(|w| w[0] == "name" && w[1] == self.group)
        });
        if !found {
            return Err(QueueError::NoHandler(topic.to_string()));
        }
        Self::lock(&self.state).known_groups.insert(topic.to_string());
        Ok(())
    }

    /// 读循环：一次读全部 topic（所有 stream key 在前、`>` 在后，空转每 block 周期
    /// 一次往返）；NOGROUP（流被删/组未建完）自动重建组后继续
    async fn run_read_loop(&self) {
        let (keys, handlers): (Vec<String>, Vec<(String, Handler)>) = {
            let st = Self::lock(&self.state);
            let handlers = st
                .topics
                .iter()
                .filter_map(|t| {
                    st.handlers
                        .get(t)
                        .cloned()
                        .map(|h| (t.clone(), h))
                })
                .collect();
            (st.topics.clone(), handlers)
        };
        if keys.is_empty() {
            return;
        }
        let mut conn = self.conn.clone();
        let opts = StreamReadOptions::default()
            .group(&self.group, &self.consumer)
            .count(self.batch)
            .block((self.block_secs * 1000) as usize);

        while !self.closed.load(Ordering::SeqCst) {
            match conn
                .xread_options::<_, _, StreamReadReply>(&keys, &[">"], &opts)
                .await
            {
                Ok(reply) => {
                    for key in reply.keys {
                        let Some((_, handler)) = handlers.iter().find(|(t, _)| *t == key.key)
                        else {
                            continue;
                        };
                        for sid in key.ids {
                            let topic = key.key.trim_start_matches(&self.key_prefix).to_string();
                            let mut values = serde_json::Map::new();
                            for (field, raw) in &sid.map {
                                if field == EMPTY_SENTINEL {
                                    continue;
                                }
                                let s = redis_val_to_string(raw);
                                match serde_json::from_str::<serde_json::Value>(&s) {
                                    Ok(v) => values.insert(field.clone(), v),
                                    Err(_) => values.insert(
                                        field.clone(),
                                        serde_json::Value::String(s.clone()),
                                    ),
                                };
                            }
                            let msg = Message {
                                id: sid.id.clone(),
                                topic,
                                values: serde_json::Value::Object(values),
                                attempts: 1,
                            };
                            let m = msg.clone();
                            match (handler)(m).await {
                                Ok(()) => {
                                    let _: Result<i64, _> = conn
                                        .xack::<_, _, _, i64>(&key.key, &self.group, &[&sid.id])
                                        .await;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        topic = %msg.topic,
                                        id = %sid.id,
                                        error = %e,
                                        "redis queue handler failed, message left pending for retry/claim"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    if e.to_string().contains("NOGROUP") {
                        // 组或流被外部删除：重建后继续（订阅与启动之间的空窗自愈）
                        for key in &keys {
                            let _: Result<(), _> = redis::cmd("XGROUP")
                                .arg("CREATE")
                                .arg(key)
                                .arg(&self.group)
                                .arg("$")
                                .arg("MKSTREAM")
                                .query_async::<()>(&mut conn)
                                .await;
                        }
                        continue;
                    }
                    // ConnectionManager 会自动重连；退避一个 block 周期防刷日志
                    tracing::error!(error = %e, "redis queue xreadgroup failed, backing off");
                    tokio::time::sleep(Duration::from_secs(self.block_secs)).await;
                }
            }
        }
    }

    /// 接管循环：周期性扫描 pending 列表，把闲置超过阈值的未 ACK 消息
    /// XCLAIM 过来重投（按 delivery 计数过滤死信，避免反复空转）
    async fn run_reclaim_loop(&self) {
        let (keys, handlers): (Vec<String>, Vec<Handler>) = {
            let st = Self::lock(&self.state);
            (
                st.topics.clone(),
                st.topics
                    .iter()
                    .filter_map(|t| st.handlers.get(t).cloned())
                    .collect(),
            )
        };
        if keys.is_empty() {
            return;
        }
        let min_idle_ms = self.claim_min_idle_secs * 1000;
        let mut conn = self.conn.clone();

        while !self.closed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(self.claim_min_idle_secs)).await;
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            for (key, handler) in keys.iter().zip(handlers.iter()) {
                // XPENDING key group IDLE min_idle - + count → (id, consumer, idle_ms, delivery)
                let pending: Result<Vec<(String, String, u64, u64)>, _> = redis::cmd("XPENDING")
                    .arg(key)
                    .arg(&self.group)
                    .arg("IDLE")
                    .arg(min_idle_ms)
                    .arg("-")
                    .arg("+")
                    .arg(self.batch)
                    .query_async(&mut conn)
                    .await;
                let Ok(pending) = pending else {
                    continue;
                };
                for (id, _consumer, _idle, delivery) in pending {
                    if delivery >= self.max_attempts {
                        continue; // 死信：留在 pending 观测，不自动接管
                    }
                    let claimed: Result<StreamClaimReply, _> = redis::cmd("XCLAIM")
                        .arg(key)
                        .arg(&self.group)
                        .arg(&self.consumer)
                        .arg(min_idle_ms)
                        .arg(&id)
                        .query_async(&mut conn)
                        .await;
                    let Ok(claimed) = claimed else {
                        continue;
                    };
                    for sid in claimed.ids {
                        let topic = key.trim_start_matches(&self.key_prefix).to_string();
                        let mut values = serde_json::Map::new();
                        for (field, raw) in &sid.map {
                            if field == EMPTY_SENTINEL {
                                continue;
                            }
                            let s = redis_val_to_string(raw);
                            match serde_json::from_str::<serde_json::Value>(&s) {
                                Ok(v) => values.insert(field.clone(), v),
                                Err(_) => values.insert(
                                    field.clone(),
                                    serde_json::Value::String(s.clone()),
                                ),
                            };
                        }
                        let msg = Message {
                            id: sid.id.clone(),
                            topic,
                            values: serde_json::Value::Object(values),
                            attempts: delivery.max(1) as u32,
                        };
                        let m = msg.clone();
                        match (handler)(m).await {
                            Ok(()) => {
                                let _: Result<i64, _> = conn
                                    .xack::<_, _, _, i64>(key, &self.group, &[&sid.id])
                                    .await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    topic = %msg.topic,
                                    id = %sid.id,
                                    attempts = msg.attempts,
                                    error = %e,
                                    "reclaimed message handler failed, left pending again"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

fn redis_val_to_string(v: &redis::Value) -> String {
    use redis::FromRedisValue;
    String::from_redis_value(v).unwrap_or_default()
}

/// 消费者名随机后缀（进程内多队列/多实例防重名；不引 uuid 依赖，取纳秒时间戳）
fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0)
}

#[async_trait::async_trait]
impl Queue for RedisQueue {
    async fn publish(&self, topic: &str, values: serde_json::Value) -> Result<String, QueueError> {
        let key = self.stream_key(topic);
        self.ensure_group_for_publish(topic, &key).await?;

        let fields: Vec<(String, String)> = match &values {
            serde_json::Value::Object(map) if !map.is_empty() => map
                .iter()
                .map(|(k, v)| (k.clone(), v.to_string()))
                .collect(),
            _ => vec![(EMPTY_SENTINEL.to_string(), String::new())],
        };

        let mut conn = self.conn.clone();
        let mut cmd = redis::cmd("XADD");
        cmd.arg(&key).arg("*");
        for (k, v) in &fields {
            cmd.arg(k).arg(v);
        }
        let id: String = cmd.query_async(&mut conn).await?;
        Ok(id)
    }

    async fn subscribe(&self, topic: &str, handler: Handler) -> Result<(), QueueError> {
        {
            let st = Self::lock(&self.state);
            if st.closed {
                return Err(QueueError::Closed);
            }
            if st.started {
                return Err(QueueError::AlreadyStarted);
            }
            if st.handlers.contains_key(topic) {
                return Err(QueueError::AlreadySubscribed(topic.to_string()));
            }
        }
        let key = self.stream_key(topic);
        let mut conn = self.conn.clone();
        // 订阅即建组（MKSTREAM 建流、`$` 从当前尾部开始不重放历史）；
        // BUSYGROUP 视为成功（组已存在）
        let res: Result<(), _> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(&key)
            .arg(&self.group)
            .arg("$")
            .arg("MKSTREAM")
            .query_async::<()>(&mut conn)
            .await;
        if let Err(e) = res {
            if !e.to_string().contains("BUSYGROUP") {
                return Err(QueueError::Redis(e));
            }
        }
        let mut st = Self::lock(&self.state);
        // 建组期间可能有并发订阅同名 topic：再查一次防覆盖
        if st.handlers.contains_key(topic) {
            return Err(QueueError::AlreadySubscribed(topic.to_string()));
        }
        st.handlers.insert(topic.to_string(), handler);
        st.topics.push(key);
        Ok(())
    }

    async fn start(self: Arc<Self>) -> Result<(), QueueError> {
        {
            let mut st = Self::lock(&self.state);
            if st.closed {
                return Err(QueueError::Closed);
            }
            if st.started {
                return Err(QueueError::AlreadyStarted);
            }
            st.started = true;
        }
        self.closed.store(false, Ordering::SeqCst);
        let read = self.clone();
        let reclaim = self.clone();
        tokio::spawn(async move { read.run_read_loop().await });
        tokio::spawn(async move { reclaim.run_reclaim_loop().await });
        Ok(())
    }

    async fn close(&self) -> Result<(), QueueError> {
        let mut st = Self::lock(&self.state);
        st.closed = true;
        drop(st);
        // 读循环最多再等一个 block 周期退出；在途 handler 完成后正常 XACK，
        // 未 ACK 的消息留在 pending 由其他实例接管（消费组语义，无需排空）
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
