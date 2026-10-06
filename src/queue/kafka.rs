//! Kafka 队列后端（feature = "queue-kafka"，rdkafka）：topic 名即 Kafka topic，
//! 消费组语义与 redis 后端一致（同组分摊、异组各收一份）。
//! ack = 提交位移（at-least-once）；nack 不提交（重启后重投，配合 Worker 重试）。
//!
//! 构建依赖：rdkafka 需要本机 CMake / C 工具链编译 librdkafka。

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::Message as KafkaMessage;

use super::{Delivery, Queue, QueueError};
use crate::config::sections::QueueKafkaSettings;

struct State {
    topics: HashSet<String>,
}

pub struct KafkaQueue {
    producer: FutureProducer,
    consumer: Arc<StreamConsumer>,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
}

impl KafkaQueue {
    /// 建连即验证（坏 broker 在装配期报错）
    pub async fn connect(settings: &QueueKafkaSettings) -> Result<Self, QueueError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &settings.brokers)
            .set("enable.idempotence", "true")
            .create()
            .map_err(|e| QueueError::Backend(format!("kafka producer failed: {e}")))?;
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &settings.brokers)
            .set("group.id", &settings.group)
            .set("enable.auto.commit", "false")
            // earliest：位移未提交（首次上线/位移丢失）时从最早消费，避免静默跳过停机窗口
            .set("auto.offset.reset", "earliest")
            .create()
            .map_err(|e| QueueError::Backend(format!("kafka consumer failed: {e}")))?;
        Ok(Self {
            producer,
            consumer: Arc::new(consumer),
            state: Arc::new(Mutex::new(State { topics: HashSet::new() })),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Queue for KafkaQueue {
    fn name(&self) -> &'static str {
        "kafka"
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> {
        {
            let mut st = KafkaQueue::lock(&self.state);
            if !st.topics.insert(topic.to_string()) {
                return Err(QueueError::AlreadyRegistered(topic.to_string()));
            }
        }
        let topics: Vec<&str> = KafkaQueue::lock(&self.state)
            .topics
            .iter()
            .map(|s| s.as_str())
            .collect();
        self.consumer
            .subscribe(&topics)
            .map_err(|e| QueueError::Backend(format!("kafka subscribe failed: {e}")))?;
        Ok(())
    }

    async fn publish(
        &self,
        topic: &str,
        payload: serde_json::Value,
        headers: BTreeMap<String, String>,
    ) -> Result<String, QueueError> {
        if !KafkaQueue::lock(&self.state).topics.contains(topic) {
            return Err(QueueError::NoHandler(topic.to_string()));
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }).to_string();
        let record = FutureRecord::to(topic).payload(&body).key("");
        let (offset, _err) = self
            .producer
            .send(record, Duration::from_secs(10))
            .await
            .map_err(|(e, _)| QueueError::Backend(format!("kafka publish failed: {e}")))?;
        Ok(offset.to_string())
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        while out.len() < max.max(1) {
            // 首条等待 1s，后续立即取积压
            let timeout = if out.is_empty() {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(1)
            };
            match tokio::time::timeout(timeout, self.consumer.recv()).await {
                Ok(Ok(msg)) => {
                    let payload = match msg.payload_view::<str>() {
                        Some(Ok(s)) => s.to_string(),
                        _ => continue,
                    };
                    let topic = msg.topic().to_string();
                    if let Some(m) = decode_message(&payload, &topic) {
                        // ack_token：partition:offset（ack 时提交）
                        let token = format!("{}:{}", msg.partition(), msg.offset());
                        out.push(Delivery {
                            ack_token: token,
                            message: m,
                        });
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "kafka receive failed");
                    break;
                }
                Err(_) => break, // 等待超时
            }
        }
        Ok(out)
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        // 提交位移到 broker（at-least-once）。只 store_offset 不 commit 的话，
        // 位移从不持久化，重启后按 auto.offset.reset 重新开始 = 停机窗口消息全丢。
        // Async 提交：进程崩溃最多重复投递（不丢），符合 at-least-once 语义。
        let (partition, offset) = delivery
            .ack_token
            .split_once(':')
            .ok_or_else(|| QueueError::Backend("bad kafka ack token".to_string()))?;
        let partition: i32 = partition
            .parse()
            .map_err(|_| QueueError::Backend("bad kafka partition".to_string()))?;
        let offset: i64 = offset
            .parse()
            .map_err(|_| QueueError::Backend("bad kafka offset".to_string()))?;
        let mut tpl = rdkafka::topic_partition_list::TopicPartitionList::new();
        tpl.add_partition_offset(
            &delivery.message.topic,
            partition,
            rdkafka::topic_partition_list::Offset::Offset(offset + 1),
        )
        .map_err(|e| QueueError::Backend(format!("kafka offset build failed: {e}")))?;
        self.consumer
            .commit(&tpl, rdkafka::consumer::CommitMode::Async)
            .map_err(|e| QueueError::Backend(format!("kafka commit failed: {e}")))?;
        Ok(())
    }

    async fn nack(&self, _delivery: &Delivery) -> Result<(), QueueError> {
        // 不提交位移：重启/重平衡后重投；Worker 重试语义不受影响
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
