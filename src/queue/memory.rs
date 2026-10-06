//! 进程内内存队列（feature = "queue-memory"，默认）：全实例一个有界 channel，
//! `receive` 直接从中拉取（Worker 的并发任务天然分摊）。失败重试与死信由
//! Worker 统一承担；重启即丢，适合开发环境与可容忍丢失的轻量任务。

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::{Delivery, Queue, QueueError};
use crate::config::sections::QueueSettings;

struct State {
    topics: HashSet<String>,
    closed: bool,
}

pub struct MemoryQueue {
    /// close 时置 None（drop sender，消费端 recv 返回 None 自然排空退出）
    tx: Mutex<Option<mpsc::Sender<crate::queue::Message>>>,
    /// tokio 异步锁：recv 阻塞期间不阻塞其他任务（多 Worker 轮流持锁等待）
    rx: tokio::sync::Mutex<Option<mpsc::Receiver<crate::queue::Message>>>,
    state: Mutex<State>,
    seq: AtomicU64,
    closed: Arc<AtomicBool>,
}

impl MemoryQueue {
    pub fn new(settings: &QueueSettings) -> Self {
        let (tx, rx) = mpsc::channel(settings.memory.buffer.max(1));
        Self {
            tx: Mutex::new(Some(tx)),
            rx: tokio::sync::Mutex::new(Some(rx)),
            state: Mutex::new(State {
                topics: HashSet::new(),
                closed: false,
            }),
            seq: AtomicU64::new(0),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Queue for MemoryQueue {
    fn name(&self) -> &'static str {
        "memory"
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> {
        let mut st = Self::lock(&self.state);
        if st.closed {
            return Err(QueueError::Closed);
        }
        if !st.topics.insert(topic.to_string()) {
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
        {
            let st = Self::lock(&self.state);
            if st.closed {
                return Err(QueueError::Closed);
            }
            if !st.topics.contains(topic) {
                return Err(QueueError::NoHandler(topic.to_string()));
            }
        }
        let mut msg = crate::queue::Message::new(topic, payload);
        msg.headers = headers;
        msg.id = self.seq.fetch_add(1, Ordering::Relaxed).to_string();
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(tx) = tx else {
            return Err(QueueError::Closed);
        };
        // 队满报错而非阻塞业务（与 go-admin-core memory 队列语义一致）
        let id = msg.id.clone();
        tx.try_send(msg)
            .map_err(|_| QueueError::Full(topic.to_string()))?;
        Ok(id)
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> {
        let mut guard = self.rx.lock().await;
        let Some(rx) = guard.as_mut() else {
            return Ok(Vec::new()); // 已关闭（close 后 sender drop，recv 返回 None 排空退出）
        };
        let mut out = Vec::new();
        for _ in 0..max.max(1) {
            // 首条阻塞等待（最多 500ms，避免长期占用锁）；拿到后改 try_recv 排空积压
            let msg = if out.is_empty() {
                match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                    Ok(m) => m,
                    Err(_) => break, // 等待超时，本轮无消息
                }
            } else {
                rx.try_recv().ok()
            };
            let Some(msg) = msg else {
                // channel 关闭：置空 receiver，后续 receive 直接返回空
                *guard = None;
                break;
            };
            out.push(Delivery {
                ack_token: String::new(),
                message: msg,
            });
        }
        Ok(out)
    }

    async fn ack(&self, _delivery: &Delivery) -> Result<(), QueueError> {
        Ok(()) // 拉取即消费，无需确认
    }

    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError> {
        // 重试语义由 Worker 承担；到达 nack 意味着重试耗尽，只能记日志丢弃
        tracing::error!(
            topic = %delivery.message.topic,
            id = %delivery.message.id,
            "memory queue message dropped after retries"
        );
        Ok(())
    }

    async fn close(&self) -> Result<(), QueueError> {
        let mut st = Self::lock(&self.state);
        if st.closed {
            return Ok(());
        }
        st.closed = true;
        st.topics.clear();
        self.closed.store(true, Ordering::SeqCst);
        // drop 全部 sender：消费者排空缓冲后自然退出（优雅排空）
        *self.tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}
