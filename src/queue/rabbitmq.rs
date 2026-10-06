//! RabbitMQ 队列后端（feature = "queue-rabbitmq"，lapin）：topic 名即队列名，
//! 经默认交换机直投（routing_key = topic）。注册 = durable 队列声明；
//! receive = basic.get 轮询；ack/nack = basic.ack / basic.reject。
//! 重试语义统一由 Worker 承担；nack reject 不重回队列（可配 DLX 的部署进死信）。

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lapin::options::{BasicAckOptions, BasicGetOptions, BasicPublishOptions, BasicQosOptions, BasicRejectOptions, ConfirmSelectOptions, QueueDeclareOptions};
use lapin::types::FieldTable;
use lapin::BasicProperties;
use lapin::{Channel, Connection, ConnectionProperties};

use super::{Delivery, Queue, QueueError};
use crate::config::sections::QueueRabbitmqSettings;

struct State {
    topics: HashSet<String>,
}

pub struct RabbitmqQueue {
    channel: Channel,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
    seq: AtomicU64,
}

impl RabbitmqQueue {
    /// 建连即验证（坏地址在装配期报错）
    pub async fn connect(settings: &QueueRabbitmqSettings) -> Result<Self, QueueError> {
        let conn = Connection::connect(
            settings.url.as_str(),
            ConnectionProperties::default(),
        )
        .await
        .map_err(|e| QueueError::Backend(format!("rabbitmq connect failed: {e}")))?;
        let channel = conn
            .create_channel()
            .await
            .map_err(|e| QueueError::Backend(format!("rabbitmq channel failed: {e}")))?;
        // publisher confirm：publish 的返回确认必须等到 broker ack，
        // 否则 broker 重启/路由失败时消息静默消失
        channel
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(|e| QueueError::Backend(format!("rabbitmq confirm mode failed: {e}")))?;
        channel
            .basic_qos(settings.prefetch.max(1), BasicQosOptions::default())
            .await
            .map_err(|e| QueueError::Backend(format!("rabbitmq qos failed: {e}")))?;
        Ok(Self {
            channel,
            state: Arc::new(Mutex::new(State { topics: HashSet::new() })),
            closed: Arc::new(AtomicBool::new(false)),
            seq: AtomicU64::new(0),
        })
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Queue for RabbitmqQueue {
    fn name(&self) -> &'static str {
        "rabbitmq"
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> {
        // durable 队列，topic 名即队列名
        self.channel
            .queue_declare(
                topic,
                QueueDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| QueueError::Backend(format!("queue declare failed: {e}")))?;
        if !RabbitmqQueue::lock(&self.state).topics.insert(topic.to_string()) {
            return Err(QueueError::AlreadyRegistered(topic.to_string()));
        }
        Ok(())
    }

    async fn publish(
        &self,
        topic: &str,
        payload: serde_json::Value,
        headers: BTreeMap<String, String>,
    ) -> Result<String, QueueError> {
        if !RabbitmqQueue::lock(&self.state).topics.contains(topic) {
            return Err(QueueError::NoHandler(topic.to_string()));
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }).to_string();
        let confirm = self
            .channel
            .basic_publish(
                "", // 默认交换机：routing_key 即队列名
                topic,
                BasicPublishOptions {
                    // mandatory：队列不存在时不可无声丢弃，让 confirm 报错
                    mandatory: true,
                    ..Default::default()
                },
                body.as_bytes(),
                // delivery_mode=2（持久化）：队列 durable 但消息瞬时的话，
                // broker 重启后队列还在、里面空了
                BasicProperties::default().with_delivery_mode(2),
            )
            .await
            .map_err(|e| QueueError::Backend(format!("publish failed: {e}")))?;
        confirm
            .await
            .map_err(|e| QueueError::Backend(format!("publish confirm failed: {e}")))?;
        Ok(self.seq.fetch_add(1, Ordering::Relaxed).to_string())
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let topics: Vec<String> = RabbitmqQueue::lock(&self.state).topics.iter().cloned().collect();
        let mut out = Vec::new();
        // basic.get 轮询各队列（简单可靠；高吞吐部署建议换 consume + acker 模式）
        for topic in &topics {
            let Ok(get) = self
                .channel
                .basic_get(topic, BasicGetOptions::default())
                .await
            else {
                continue;
            };
            let Some(get) = get else { continue };
            let body = String::from_utf8_lossy(get.data.as_slice()).to_string();
            if let Some(msg) = decode_message(&body, topic) {
                out.push(Delivery {
                    ack_token: get.delivery_tag.to_string(),
                    message: msg,
                });
            } else {
                let _ = get.acker.ack(BasicAckOptions::default()).await;
            }
            if out.len() >= max.max(1) {
                break;
            }
        }
        if out.is_empty() {
            // 无消息：小睡后返回空（Worker 循环会重试）
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Ok(out)
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        let tag: u64 = delivery
            .ack_token
            .parse()
            .map_err(|_| QueueError::Backend("bad delivery tag".to_string()))?;
        self.channel
            .basic_ack(tag, BasicAckOptions::default())
            .await
            .map_err(|e| QueueError::Backend(format!("ack failed: {e}")))?;
        Ok(())
    }

    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        let tag: u64 = delivery
            .ack_token
            .parse()
            .map_err(|_| QueueError::Backend("bad delivery tag".to_string()))?;
        self.channel
            .basic_reject(tag, BasicRejectOptions { requeue: false })
            .await
            .map_err(|e| QueueError::Backend(format!("reject failed: {e}")))?;
        Ok(())
    }

    async fn close(&self) -> Result<(), QueueError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn decode_message(body: &str, topic: &str) -> Option<crate::queue::Message> {
    #[derive(serde::Deserialize)]
    struct Body {
        payload: serde_json::Value,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    }
    let parsed: Body = serde_json::from_str(body).ok()?;
    let mut msg = crate::queue::Message::new(topic, parsed.payload);
    msg.headers = parsed.headers;
    Some(msg)
}
