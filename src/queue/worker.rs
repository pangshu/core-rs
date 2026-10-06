//! Worker：把消费端封装成统一形态（文档 三·12）——注册 handler、并发消费、
//! 失败重试（指数退避 + 抖动）、死信转发、随 App 优雅停机。
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # use core_rs::queue::worker::Worker;
//! # async fn demo(queue: QueueHandle) -> Result<(), QueueError> {
//! let worker = Worker::new(queue)
//!     .consumer("email.send", |msg| async move {
//!         tracing::info!(payload = %msg.payload, "sending email");
//!         Ok(())
//!     })
//!     .consumer("video.transcode", |msg| async move { Ok(()) });
//! worker.start().await?;   // App::serve 内部调用并持有停机句柄
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::{Delivery, QueueError, QueueHandle};

/// 消费回调
pub type Handler = Arc<
    dyn Fn(crate::queue::Message) -> Pin<Box<dyn Future<Output = Result<(), QueueError>> + Send>>
        + Send
        + Sync,
>;

/// 消费者注册表 + 运行参数
pub struct Worker {
    queue: QueueHandle,
    handlers: HashMap<String, Handler>,
    concurrency: usize,
    max_attempts: u32,
    backoff_base: Duration,
    dead_letter_topic: String,
}

impl Worker {
    pub fn new(queue: QueueHandle) -> Self {
        Self {
            queue,
            handlers: HashMap::new(),
            concurrency: 4,
            max_attempts: 3,
            backoff_base: Duration::from_millis(1000),
            dead_letter_topic: String::new(),
        }
    }

    /// 覆盖 `[queue]` 配置的并发度
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// 覆盖 `[queue]` 配置的重试次数
    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    /// 覆盖 `[queue]` 配置的重试基础退避
    pub fn retry_backoff_ms(mut self, ms: u64) -> Self {
        self.backoff_base = Duration::from_millis(ms.max(1));
        self
    }

    /// 覆盖 `[queue]` 配置的死信 topic
    pub fn dead_letter_topic(mut self, topic: impl Into<String>) -> Self {
        self.dead_letter_topic = topic.into();
        self
    }

    /// 注册 topic handler（必须在 start 之前；重复注册后者覆盖）
    pub fn consumer<F, Fut>(mut self, topic: impl Into<String>, f: F) -> Self
    where
        F: Fn(crate::queue::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), QueueError>> + Send + 'static,
    {
        self.handlers
            .insert(topic.into(), Arc::new(move |msg| Box::pin(f(msg))));
        self
    }

    /// 注册全部 topic 并启动消费任务；返回的 [`WorkerRunner`] 由 App 持有，
    /// 进程退出前调用 `shutdown()`（排空在途消息后退出）。
    pub async fn start(self) -> Result<WorkerRunner, QueueError> {
        // 先全部订阅再启动（redis 后端订阅即建组，启动与订阅之间的消息不丢）
        let topics: Vec<String> = self.handlers.keys().cloned().collect();
        for topic in &topics {
            self.queue.register(topic).await?;
        }
        tracing::info!(
            backend = self.queue.name(),
            topics = ?topics,
            concurrency = self.concurrency,
            max_attempts = self.max_attempts,
            "queue worker starting"
        );

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let mut tasks = Vec::with_capacity(self.concurrency);
        for i in 0..self.concurrency {
            let worker = Arc::new(self.clone_for_task());
            let mut shutdown = stop_rx.clone();
            tasks.push(tokio::spawn(async move {
                worker.run_task(i, &mut shutdown).await;
            }));
        }
        Ok(WorkerRunner {
            stop_tx,
            tasks,
            queue: self.queue,
        })
    }

    /// 每个并发任务共用注册表，但持有独立的迭代状态
    fn clone_for_task(&self) -> TaskWorker {
        TaskWorker {
            queue: self.queue.clone(),
            handlers: self
                .handlers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            max_attempts: self.max_attempts,
            backoff_base: self.backoff_base,
            dead_letter_topic: self.dead_letter_topic.clone(),
        }
    }
}

/// 任务侧：单一 receive → dispatch → retry 循环
#[derive(Clone)]
struct TaskWorker {
    queue: QueueHandle,
    handlers: HashMap<String, Handler>,
    max_attempts: u32,
    backoff_base: Duration,
    dead_letter_topic: String,
}

impl TaskWorker {
    async fn run_task(&self, task_id: usize, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
        let mut in_flight = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                batch = self.queue.receive(16) => match batch {
                    Ok(deliveries) if deliveries.is_empty() => continue,
                    Ok(deliveries) => {
                        // 串行 for + 退避同步 sleep 会让一条毒消息霸占整个任务
                        //（退避期间不拉新消息、同批其它消息排队）；每条投递独立任务
                        for d in deliveries {
                            let worker = self.clone();
                            in_flight.spawn(async move {
                                worker.dispatch(task_id, d).await;
                            });
                        }
                    }
                    Err(e) => {
                        tracing::error!(task = task_id, error = %e, "queue receive failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
        // 优雅停机：等在途投递（含退避中的重试）结束后再退出
        while in_flight.join_next().await.is_some() {}
    }

    async fn dispatch(&self, task_id: usize, delivery: Delivery) {
        let msg = &delivery.message;
        let Some(handler) = self.handlers.get(&msg.topic) else {
            // publish 只接受已注册 topic，正常不会走到这里
            tracing::warn!(topic = %msg.topic, id = %msg.id, "no handler, discarding");
            let _ = self.queue.ack(&delivery).await;
            return;
        };

        let rounds = self.max_attempts.max(1);
        let mut last_err: Option<QueueError> = None;
        for attempt in 1..=rounds {
            let mut m = msg.clone();
            m.attempts = attempt;
            match (handler)(m).await {
                Ok(()) => {
                    let _ = self.queue.ack(&delivery).await;
                    return;
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < rounds {
                        let backoff = self.backoff(
                            attempt,
                        );
                        tracing::warn!(
                            task = task_id,
                            topic = %msg.topic,
                            id = %msg.id,
                            attempt,
                            error = %last_err.as_ref().unwrap(),
                            "queue handler failed, retrying"
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        let err = last_err.unwrap_or(QueueError::Backend("unknown".to_string()));
        tracing::error!(
            topic = %msg.topic,
            id = %msg.id,
            attempts = rounds,
            error = %err,
            "queue handler failed permanently"
        );
        // 重试耗尽：配置了死信且转发成功 → ack（消息已妥善安置）；
        // 未配置死信（或转发失败）→ 只 nack 交由后端处置（redis 留 pending、
        // rabbitmq DLX/reject、kafka 不提交位移）。nack 后再 ack 在 rabbitmq 上
        // 会因 delivery tag 已结算触发 PRECONDITION_FAILED 毒化整个 channel。
        let mut dead_lettered = false;
        if !self.dead_letter_topic.is_empty() && self.dead_letter_topic != msg.topic {
            let mut dead = msg.clone();
            dead.topic = self.dead_letter_topic.clone();
            dead.attempts = rounds;
            match self
                .queue
                .publish(&dead.topic, dead.payload, dead.headers)
                .await
            {
                Ok(_) => dead_lettered = true,
                Err(e) => tracing::error!(error = %e, "dead letter publish failed, leaving message unacked"),
            }
        }
        if dead_lettered {
            let _ = self.queue.ack(&delivery).await;
        } else {
            let _ = self.queue.nack(&delivery).await;
        }
    }

    /// 指数退避 + 抖动：base * 2^(attempt-1)，上限 60s
    fn backoff(&self, attempt: u32) -> Duration {
        let exp = self
            .backoff_base
            .saturating_mul(1u32.wrapping_shl(attempt.saturating_sub(1)).max(1));
        let capped = exp.min(Duration::from_secs(60));
        let jitter = rand_jitter(capped);
        capped + jitter
    }
}

fn rand_jitter(d: Duration) -> Duration {
    // 轻量抖动：0~9.9% 随机；避免引 rand，时间熵足够
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_nanos(d.as_nanos() as u64 * (nanos % 100) / 100)
}

/// Worker 运行句柄：App 持有到进程结束；`shutdown()` 置位后各任务排空退出
pub struct WorkerRunner {
    stop_tx: tokio::sync::watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    queue: QueueHandle,
}

impl WorkerRunner {
    /// 优雅停机：停止拉取 → 等待在途任务结束 → 关闭后端
    pub async fn shutdown(self) {
        let _ = self.stop_tx.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
        let _ = self.queue.close().await;
    }

    pub fn backend_name(&self) -> &'static str {
        self.queue.name()
    }
}
