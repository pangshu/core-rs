//! Redis Stream 队列后端（feature = "queue-redis"）：消费组 + ACK，at-least-once。
//!
//! - 发布：`XADD`（payload/headers 打包为单字段 JSON，跨后端无损）；
//! - 注册：`XGROUP CREATE ... MKSTREAM`（订阅即建组，注册与启动之间发布的消息不丢）；
//! - 消费：`XREADGROUP GROUP g c COUNT n BLOCK t STREAMS k1 k2 >`（同组多实例
//!   负载均衡分摊，异组各收一份）；每轮先 `XAUTOCLAIM` 接管闲置 pending
//!   （消费者宕机自愈，需 Redis 6.2+）；
//! - 确认：Worker 成功后 `XACK`；失败不 ACK，pending 里的消息闲置超过
//!   `claim_min_idle_secs` 被任意实例接手重投；
//! - 死信：重试耗尽的消息由 Worker 转发死信 topic 后 ACK；无死信 topic 时
//!   留在 pending 列表（XPENDING 可观测），人工处理后 XACK/XDEL。

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use redis::aio::ConnectionManager;
use redis::AsyncCommands;

use super::{Delivery, Queue, QueueError};
use crate::config::sections::QueueRedisSettings;

/// 消息打包字段名（XADD 拒绝全空 entry）
const FIELD: &str = "m";

struct State {
    topics: Vec<String>,
    known_groups: HashSet<String>,
}

pub struct RedisQueue {
    conn: ConnectionManager,
    group: String,
    consumer: String,
    key_prefix: String,
    block_secs: u64,
    claim_min_idle_secs: u64,
    max_attempts: u64,
    batch: usize,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
}

impl RedisQueue {
    /// 建连即验证（坏地址在装配期报错，而非首条消息时）
    pub async fn new(settings: &QueueRedisSettings) -> Result<Self, QueueError> {
        let client = redis::Client::open(settings.url.as_str())
            .map_err(|e| QueueError::Backend(format!("bad redis url: {e}")))?;
        // ConnectionManager：断线自动重连
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| QueueError::Backend(format!("redis connect failed: {e}")))?;
        let consumer = if settings.consumer.is_empty() {
            format!(
                "{}-{}-{:x}",
                std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string()),
                std::process::id(),
                crate::utils::time::now_ms()
            )
        } else {
            settings.consumer.clone()
        };
        Ok(Self {
            conn,
            group: if settings.group.is_empty() {
                "core-rs".to_string()
            } else {
                settings.group.clone()
            },
            consumer,
            key_prefix: settings.key_prefix.clone(),
            block_secs: settings.block_secs.max(1),
            claim_min_idle_secs: settings.claim_min_idle_secs.max(1),
            max_attempts: settings.max_attempts.max(1),
            batch: settings.batch.clamp(1, 1000),
            state: Arc::new(Mutex::new(State {
                topics: Vec::new(),
                known_groups: HashSet::new(),
            })),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn stream_key(&self, topic: &str) -> String {
        format!("{}{}", self.key_prefix, topic)
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 接管闲置 pending（宕机自愈，需 Redis 6.2+；失败只记 debug，不影响新消息消费）。
    /// 先 XPENDING 读 `times_delivered`：超过 `max_attempts` 的不再接管（留 pending
    /// 告警），否则 ack 失败的消息会无限重投。之后 XCLAIM 接管并取回 payload。
    async fn claim_idle(
        &self,
        conn: &mut ConnectionManager,
        key: &str,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let min_idle = self.claim_min_idle_secs * 1000;
        let reply: Result<redis::Value, _> = redis::cmd("XPENDING")
            .arg(key)
            .arg(&self.group)
            .arg("IDLE")
            .arg(min_idle)
            .arg("-")
            .arg("+")
            .arg(self.batch)
            .query_async(conn)
            .await;
        let mut ids = Vec::new();
        match reply {
            Ok(redis::Value::Array(entries)) => {
                for entry in entries {
                    // 每条：[id, consumer, idle_ms, times_delivered]
                    if let redis::Value::Array(item) = entry {
                        if item.len() >= 4 {
                            let id = match &item[0] {
                                redis::Value::BulkString(d) => {
                                    String::from_utf8_lossy(d).to_string()
                                }
                                _ => continue,
                            };
                            let times_delivered = match &item[3] {
                                redis::Value::Int(n) => *n as u64,
                                _ => 0,
                            };
                            if times_delivered > self.max_attempts {
                                tracing::warn!(
                                    key = %key,
                                    id = %id,
                                    times_delivered,
                                    "pending message exceeded queue.redis.max_attempts, left for manual handling"
                                );
                                continue;
                            }
                            ids.push(id);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(key = %key, error = %e, "XPENDING unavailable, skip idle reclaim");
                return out;
            }
        }
        if ids.is_empty() {
            return out;
        }
        let mut cmd = redis::cmd("XCLAIM");
        cmd.arg(key).arg(&self.group).arg(&self.consumer).arg(min_idle);
        for id in &ids {
            cmd.arg(id);
        }
        match cmd.query_async::<redis::Value>(conn).await {
            Ok(redis::Value::Array(entries)) => {
                for entry in entries {
                    if let redis::Value::Array(item) = entry {
                        if item.len() >= 2 {
                            let id = match &item[0] {
                                redis::Value::BulkString(d) => String::from_utf8_lossy(d).to_string(),
                                _ => continue,
                            };
                            if let Some(payload) = extract_field(&item[1], FIELD) {
                                out.push((id, payload));
                            }
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(key = %key, error = %e, "XCLAIM unavailable, skip idle reclaim")
            }
        }
        out
    }
}

/// 从 XADD/XCLAIM 的 field-value 扁平数组中取指定字段
fn extract_field(fv: &redis::Value, field: &str) -> Option<String> {
    let fv = match fv {
        redis::Value::Array(b) => b,
        _ => return None,
    };
    for pair in fv.chunks(2) {
        if pair.len() == 2 {
            if let redis::Value::BulkString(d) = &pair[0] {
                if d == field.as_bytes() {
                    return match &pair[1] {
                        redis::Value::BulkString(v) => Some(String::from_utf8_lossy(v).to_string()),
                        _ => None,
                    };
                }
            }
        }
    }
    None
}

#[async_trait::async_trait]
impl Queue for RedisQueue {
    fn name(&self) -> &'static str {
        "redis"
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> {
        let key = self.stream_key(topic);
        let mut conn = self.conn.clone();
        // XGROUP CREATE ... MKSTREAM；BUSYGROUP = 组已存在，幂等
        let res: Result<String, _> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(&key)
            .arg(&self.group)
            .arg("$")
            .arg("MKSTREAM")
            .query_async(&mut conn)
            .await;
        match res {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("BUSYGROUP") {
                    return Err(QueueError::Backend(format!("XGROUP CREATE failed: {msg}")));
                }
            }
        }
        let mut st = Self::lock(&self.state);
        if !st.topics.contains(&key) {
            // 存全名 stream key：消费侧（receive）直接拿它 XAUTOCLAIM/XREADGROUP——
            // 此前存裸 topic，组建立在带前缀的 key 上而消费读裸 key，NOGROUP 静默空转
            st.topics.push(key.clone());
        }
        st.known_groups.insert(topic.to_string());
        Ok(())
    }

    async fn publish(
        &self,
        topic: &str,
        payload: serde_json::Value,
        headers: BTreeMap<String, String>,
    ) -> Result<String, QueueError> {
        if !Self::lock(&self.state).known_groups.contains(topic) {
            return Err(QueueError::NoHandler(topic.to_string()));
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers });
        let mut conn = self.conn.clone();
        let id: String = redis::cmd("XADD")
            .arg(self.stream_key(topic))
            .arg("*")
            .arg(FIELD)
            .arg(body.to_string())
            .query_async(&mut conn)
            .await
            .map_err(|e| QueueError::Backend(format!("XADD failed: {e}")))?;
        Ok(id)
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let (keys, ) = {
            let st = Self::lock(&self.state);
            (st.topics.clone(),)
        };
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.conn.clone();
        let mut out = Vec::new();

        // 1) 接管闲置 pending（宕机自愈）。claim 已把消息所有权转到本消费者，
        //    不能因超出 max 而 truncate 丢弃——全部交给 worker 处理
        for key in &keys {
            let topic = key
                .trim_start_matches(&self.key_prefix)
                .to_string();
            for (id, payload) in self.claim_idle(&mut conn, key).await {
                if let Some(msg) = decode_message(&id, &topic, &payload) {
                    out.push(Delivery {
                        ack_token: id,
                        message: msg,
                    });
                }
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }

        // 2) XREADGROUP 新消息（BLOCK 超时即本轮空）
        let opts = redis::streams::StreamReadOptions::default()
            .group(&self.group, &self.consumer)
            .count(max.max(1))
            .block((self.block_secs * 1000) as usize);
        let reply: Result<redis::streams::StreamReadReply, _> = conn
            .xread_options::<_, _, redis::streams::StreamReadReply>(&keys, &[">"], &opts)
            .await;
        match reply {
            Ok(data) => {
                for stream in data.keys {
                    let topic = stream
                        .key
                        .trim_start_matches(&self.key_prefix)
                        .to_string();
                    for entry in stream.ids {
                        let payload = entry.map.get(FIELD).and_then(|v| match v {
                            redis::Value::BulkString(d) => Some(String::from_utf8_lossy(d).to_string()),
                            _ => None,
                        });
                        if let Some(payload) = payload {
                            if let Some(msg) = decode_message(&entry.id, &topic, &payload) {
                                out.push(Delivery {
                                    ack_token: entry.id,
                                    message: msg,
                                });
                            }
                        }
                    }
                }
            }
            // NOGROUP（流被删/组未建完）等错误：记日志，下一轮 register 重建
            Err(e) => {
                tracing::warn!(error = %e, "XREADGROUP failed");
            }
        }
        Ok(out)
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        if delivery.ack_token.is_empty() {
            return Ok(());
        }
        let key = self.stream_key(&delivery.message.topic);
        let mut conn = self.conn.clone();
        let _: i64 = redis::cmd("XACK")
            .arg(key)
            .arg(&self.group)
            .arg(&delivery.ack_token)
            .query_async(&mut conn)
            .await
            .map_err(|e| QueueError::Backend(format!("XACK failed: {e}")))?;
        Ok(())
    }

    async fn nack(&self, _delivery: &Delivery) -> Result<(), QueueError> {
        // 不 ACK：消息留在 pending，闲置后由任意实例 XAUTOCLAIM 接管；
        // Worker 重试耗尽时若配置了死信 topic 会先转发再 ACK
        Ok(())
    }

    async fn close(&self) -> Result<(), QueueError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn ping(&self) -> Result<(), QueueError> {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| QueueError::Backend(e.to_string()))?;
        Ok(())
    }
}

fn decode_message(id: &str, topic: &str, payload: &str) -> Option<crate::queue::Message> {
    #[derive(serde::Deserialize)]
    struct Body {
        payload: serde_json::Value,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    }
    let body: Body = match serde_json::from_str(payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(id = %id, topic = %topic, error = %e, "queue message corrupt, dropped");
            return None;
        }
    };
    let mut msg = crate::queue::Message::new(topic, body.payload);
    msg.id = id.to_string();
    msg.headers = body.headers;
    Some(msg)
}
