//! NATS 队列后端（feature = "queue-nats"，async-nats + JetStream）：
//! stream/durable consumer 固定（配置节声明），topic 即 subject filter。
//! ack = message.ack()；nack = message.nak()（服务端按消费策略重投）。

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::jetstream::{self, consumer::PullConsumer};
use async_nats::Client;
use futures::StreamExt as _;

use super::{Delivery, Queue, QueueError};
use crate::config::sections::QueueNatsSettings;

struct State {
    topics: HashSet<String>,
}

pub struct NatsQueue {
    client: Client,
    js: jetstream::Context,
    consumer: tokio::sync::Mutex<PullConsumer>,
    /// 已投递未确认的消息（ack_token → 消息对象；ack/nack 需要原始对象）
    pending: tokio::sync::Mutex<std::collections::HashMap<String, jetstream::Message>>,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
}

impl NatsQueue {
    /// 建连即验证（坏地址在装配期报错）
    pub async fn connect(settings: &QueueNatsSettings) -> Result<Self, QueueError> {
        let client = async_nats::connect(settings.url.as_str())
            .await
            .map_err(|e| QueueError::Backend(format!("nats connect failed: {e}")))?;
        let js = jetstream::new(client.clone());

        // stream：subjects 全收（ *> ），按 topic 过滤在 consumer 上做
        let stream = js
            .get_or_create_stream(jetstream::stream::Config {
                name: settings.stream.clone(),
                subjects: vec!["core-rs.>".to_string()],
                ..Default::default()
            })
            .await
            .map_err(|e| QueueError::Backend(format!("nats stream failed: {e}")))?;
        // durable pull consumer（空 filter：receive 侧按已注册 subject 分发）；
        // max_deliver 必须设置：默认无限重投会让毒消息永久循环
        let consumer = stream
            .get_or_create_consumer(
                &settings.durable,
                jetstream::consumer::pull::Config {
                    durable_name: Some(settings.durable.clone()),
                    filter_subject: "core-rs.>".to_string(),
                    max_deliver: settings.max_deliver.max(1),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| QueueError::Backend(format!("nats consumer failed: {e}")))?;

        Ok(Self {
            client,
            js,
            consumer: tokio::sync::Mutex::new(consumer),
            pending: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            state: Arc::new(Mutex::new(State { topics: HashSet::new() })),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn subject(topic: &str) -> String {
        // NATS subject 不允许 '/' 等字符：统一前缀 + '.' 分隔
        format!("core-rs.{topic}")
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Queue for NatsQueue {
    fn name(&self) -> &'static str {
        "nats"
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> {
        if !NatsQueue::lock(&self.state).topics.insert(topic.to_string()) {
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
        if !NatsQueue::lock(&self.state).topics.contains(topic) {
            return Err(QueueError::NoHandler(topic.to_string()));
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }).to_string();
        let ack = self
            .js
            .publish(Self::subject(topic), body.into_bytes().into())
            .await
            .map_err(|e| QueueError::Backend(format!("nats publish failed: {e}")))?;
        let ack = ack
            .await
            .map_err(|e| QueueError::Backend(format!("nats publish ack failed: {e}")))?;
        Ok(ack.sequence.to_string())
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let consumer = self.consumer.lock().await;
        let batch = max.clamp(1, 100);
        let mut messages = consumer
            .batch()
            .max_messages(batch)
            .expires(Duration::from_millis(500))
            .messages()
            .await
            .map_err(|e| QueueError::Backend(format!("nats fetch failed: {e}")))?;
        let mut out = Vec::new();
        while let Some(msg) = messages.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "nats message error");
                    continue;
                }
            };
            // subject "core-rs.<topic>" → 还原 topic（'.' 分隔的原 topic 保留原样首个段）
            let subject = msg.subject.as_str();
            let topic = subject
                .strip_prefix("core-rs.")
                .unwrap_or(subject)
                .to_string();
            let body = String::from_utf8_lossy(&msg.message.payload).to_string();
            if let Some(m) = decode_message(&body, &topic) {
                let token = msg
                    .info()
                    .ok()
                    .map(|i| i.stream_sequence.to_string())
                    .unwrap_or_default();
                self.pending.lock().await.insert(token.clone(), msg.clone());
                out.push(Delivery {
                    ack_token: token,
                    message: m,
                });
            }
            if out.len() >= batch {
                break;
            }
        }
        Ok(out)
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        if let Some(msg) = self.pending.lock().await.remove(&delivery.ack_token) {
            msg.ack()
                .await
                .map_err(|e| QueueError::Backend(format!("nats ack failed: {e}")))?;
        }
        Ok(())
    }

    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        if let Some(msg) = self.pending.lock().await.remove(&delivery.ack_token) {
            // Nak 附带延迟：立即重投会让失败消息在窗口内高频打转
            msg.ack_with(async_nats::jetstream::AckKind::Nak(Some(
                Duration::from_secs(2),
            )))
            .await
            .map_err(|e| QueueError::Backend(format!("nats nak failed: {e}")))?;
        }
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

#[allow(dead_code)]
fn _client_alive(q: &NatsQueue) -> &Client {
    &q.client
}
