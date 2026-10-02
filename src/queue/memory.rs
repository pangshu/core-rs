//! 进程内内存队列：每 topic 一个有界 channel + 消费协程，handler 失败按
//! `max_attempts` 重试（退避 1s/2s/3s…），超过后记日志放弃。重启即丢，
//! 适合开发环境与可容忍丢失的轻量任务。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use super::{Handler, Message, Queue, QueueError};
use crate::config::QueueMemoryConfig;

struct Topic {
    tx: mpsc::Sender<Message>,
    handler: Handler,
    /// start() 时取走用于 spawn 消费协程
    rx: Option<mpsc::Receiver<Message>>,
}

#[derive(Default)]
struct State {
    topics: HashMap<String, Topic>,
    started: bool,
    closed: bool,
}

pub struct MemoryQueue {
    buffer: usize,
    max_attempts: u32,
    state: Mutex<State>,
    seq: AtomicU64,
    running: Arc<AtomicBool>,
}

impl MemoryQueue {
    pub fn new(cfg: &QueueMemoryConfig) -> Self {
        Self {
            buffer: cfg.buffer.max(1),
            max_attempts: cfg.max_attempts,
            state: Mutex::new(State::default()),
            seq: AtomicU64::new(0),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Queue for MemoryQueue {
    async fn publish(&self, topic: &str, values: serde_json::Value) -> Result<String, QueueError> {
        let (tx, id) = {
            let st = Self::lock(&self.state);
            if st.closed {
                return Err(QueueError::Closed);
            }
            let Some(t) = st.topics.get(topic) else {
                return Err(QueueError::NoHandler(topic.to_string()));
            };
            let id = self.seq.fetch_add(1, Ordering::Relaxed).to_string();
            (t.tx.clone(), id)
        };
        let msg = Message {
            id: id.clone(),
            topic: topic.to_string(),
            values,
            attempts: 1,
        };
        // 队满报错而非阻塞业务（与 go-admin-core memory 队列语义一致）
        tx.try_send(msg)
            .map_err(|_| QueueError::Full(topic.to_string()))?;
        Ok(id)
    }

    async fn subscribe(&self, topic: &str, handler: Handler) -> Result<(), QueueError> {
        let mut st = Self::lock(&self.state);
        if st.closed {
            return Err(QueueError::Closed);
        }
        if st.started {
            return Err(QueueError::AlreadyStarted);
        }
        if st.topics.contains_key(topic) {
            return Err(QueueError::AlreadySubscribed(topic.to_string()));
        }
        let (tx, rx) = mpsc::channel(self.buffer);
        st.topics.insert(
            topic.to_string(),
            Topic {
                tx,
                handler,
                rx: Some(rx),
            },
        );
        Ok(())
    }

    async fn start(self: Arc<Self>) -> Result<(), QueueError> {
        let mut st = Self::lock(&self.state);
        if st.closed {
            return Err(QueueError::Closed);
        }
        if st.started {
            return Err(QueueError::AlreadyStarted);
        }
        st.started = true;
        self.running.store(true, Ordering::SeqCst);
        for (topic, t) in st.topics.iter_mut() {
            let Some(rx) = t.rx.take() else {
                continue;
            };
            let handler = t.handler.clone();
            let name = topic.clone();
            let max_attempts = self.max_attempts;
            let running = self.running.clone();
            tokio::spawn(async move {
                consume(name, rx, handler, max_attempts, running).await;
            });
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), QueueError> {
        let mut st = Self::lock(&self.state);
        if st.closed {
            return Ok(());
        }
        st.closed = true;
        self.running.store(false, Ordering::SeqCst);
        // drop 全部 sender：消费者排空缓冲后自然退出（优雅排空）
        st.topics.clear();
        Ok(())
    }
}

/// 消费循环：排空 channel（close 后 sender 全部 drop，recv 返回 None 退出），
/// 每条消息失败按退避重试，超过 max_attempts 记日志放弃
async fn consume(
    topic: String,
    mut rx: mpsc::Receiver<Message>,
    handler: Handler,
    max_attempts: u32,
    running: Arc<AtomicBool>,
) {
    while let Some(mut msg) = rx.recv().await {
        let rounds = max_attempts.max(1);
        for attempt in 1..=rounds {
            msg.attempts = attempt;
            match (handler)(msg.clone()).await {
                Ok(()) => break,
                Err(e) if attempt < rounds => {
                    tracing::warn!(
                        topic = %topic,
                        id = %msg.id,
                        attempt,
                        error = %e,
                        "queue handler failed, retrying"
                    );
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                    if !running.load(Ordering::SeqCst) {
                        return;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        topic = %topic,
                        id = %msg.id,
                        attempts = attempt,
                        error = %e,
                        "queue handler failed permanently, message dropped"
                    );
                }
            }
        }
    }
}
