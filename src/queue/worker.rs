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

use std::collections::HashMap; // 引入哈希表存放 topic → handler 映射
use std::future::Future; // 引入 Future trait，用于 handler 返回类型约束
use std::pin::Pin; // 引入 Pin，用于装箱的异步返回
use std::sync::Arc; // 引入 Arc，用于跨任务共享 handler
use std::time::Duration; // 引入 Duration，表示退避时长

use super::{Delivery, QueueError, QueueHandle}; // 引入投递、错误与队列句柄类型

/// 消费回调
pub type Handler = Arc< // 定义消费回调类型（线程安全、可克隆）
    dyn Fn(crate::queue::Message) -> Pin<Box<dyn Future<Output = Result<(), QueueError>> + Send>> // 接收消息返回可发送的装箱 Future
        + Send // 回调需可跨线程发送
        + Sync, // 回调需可跨线程共享引用
>;

/// 消费者注册表 + 运行参数
pub struct Worker { // 定义 Worker 构建器
    queue: QueueHandle, // 绑定的队列后端句柄
    handlers: HashMap<String, Handler>, // topic → 消费回调映射
    concurrency: usize, // 并发消费任务数
    max_attempts: u32, // 单条消息最大投递次数
    backoff_base: Duration, // 重试基础退避时长
    dead_letter_topic: String, // 死信转发目标 topic（空表示不转发）
}

impl Worker {
    pub fn new(queue: QueueHandle) -> Self { // 以默认参数构造 Worker
        Self { // 填充默认运行参数
            queue, // 使用传入的队列句柄
            handlers: HashMap::new(), // 初始无 handler
            concurrency: 4, // 默认并发 4
            max_attempts: 3, // 默认最多 3 次投递
            backoff_base: Duration::from_millis(1000), // 默认退避基数 1s
            dead_letter_topic: String::new(), // 默认不启用死信
        }
    }

    /// 覆盖 `[queue]` 配置的并发度
    pub fn concurrency(mut self, n: usize) -> Self { // 设置并发度（构建器）
        self.concurrency = n.max(1); // 至少为 1
        self // 返回自身以支持链式调用
    }

    /// 覆盖 `[queue]` 配置的重试次数
    pub fn max_attempts(mut self, n: u32) -> Self { // 设置最大投递次数
        self.max_attempts = n; // 直接采用传入值
        self // 返回自身以支持链式调用
    }

    /// 覆盖 `[queue]` 配置的重试基础退避
    pub fn retry_backoff_ms(mut self, ms: u64) -> Self { // 设置重试基础退避毫秒
        self.backoff_base = Duration::from_millis(ms.max(1)); // 至少 1ms
        self // 返回自身以支持链式调用
    }

    /// 覆盖 `[queue]` 配置的死信 topic
    pub fn dead_letter_topic(mut self, topic: impl Into<String>) -> Self { // 设置死信转发目标
        self.dead_letter_topic = topic.into(); // 转为 String 保存
        self // 返回自身以支持链式调用
    }

    /// 注册 topic handler（必须在 start 之前；重复注册后者覆盖）
    pub fn consumer<F, Fut>(mut self, topic: impl Into<String>, f: F) -> Self // 注册一个 topic 的消费回调
    where // 泛型约束如下
        F: Fn(crate::queue::Message) -> Fut + Send + Sync + 'static, // 回调函数约束
        Fut: Future<Output = Result<(), QueueError>> + Send + 'static, // 返回的 Future 约束
    {
        self.handlers // 向映射中插入
            .insert(topic.into(), Arc::new(move |msg| Box::pin(f(msg)))); // 包装成装箱 Future 并存入
        self // 返回自身以支持链式调用
    }

    /// 注册全部 topic 并启动消费任务；返回的 [`WorkerRunner`] 由 App 持有，
    /// 进程退出前调用 `shutdown()`（排空在途消息后退出）。
    pub async fn start(self) -> Result<WorkerRunner, QueueError> { // 启动消费任务
        // 先全部订阅再启动（redis 后端订阅即建组，启动与订阅之间的消息不丢）
        let topics: Vec<String> = self.handlers.keys().cloned().collect(); // 取出全部已注册 topic
        for topic in &topics { // 逐个注册到后端
            self.queue.register(topic).await?; // 注册 topic（失败即中止启动）
        }
        tracing::info!( // 打印启动日志
            backend = self.queue.name(), // 记录后端名
            topics = ?topics, // 记录订阅的 topic 列表
            concurrency = self.concurrency, // 记录并发度
            max_attempts = self.max_attempts, // 记录最大投递次数
            "queue worker starting" // 日志文案
        );

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false); // 创建停机信号通道
        let mut tasks = Vec::with_capacity(self.concurrency); // 预分配任务句柄容器
        for i in 0..self.concurrency { // 为每个并发位创建任务
            let worker = Arc::new(self.clone_for_task()); // 克隆出任务侧独立状态
            let mut shutdown = stop_rx.clone(); // 克隆停机信号接收端
            tasks.push(tokio::spawn(async move { // 启动后台消费任务
                worker.run_task(i, &mut shutdown).await; // 运行该任务的消费循环
            }));
        }
        Ok(WorkerRunner { // 返回运行句柄
            stop_tx, // 停机信号发送端
            tasks, // 已启动的任务句柄
            queue: self.queue, // 队列句柄供关闭时使用
        })
    }

    /// 每个并发任务共用注册表，但持有独立的迭代状态
    fn clone_for_task(&self) -> TaskWorker { // 构造单任务侧状态
        TaskWorker { // 克隆必要字段
            queue: self.queue.clone(), // 克隆队列句柄
            handlers: self // 克隆 handler 映射
                .handlers // 访问映射
                .iter() // 遍历
                .map(|(k, v)| (k.clone(), v.clone())) // 逐项克隆键值
                .collect(), // 收集为新映射
            max_attempts: self.max_attempts, // 复制最大投递次数
            backoff_base: self.backoff_base, // 复制退避基数
            dead_letter_topic: self.dead_letter_topic.clone(), // 克隆死信 topic
        }
    }
}

/// 任务侧：单一 receive → dispatch → retry 循环
#[derive(Clone)] // 派生 Clone，便于每条投递克隆出独立任务
struct TaskWorker { // 单个并发任务侧的状态
    queue: QueueHandle, // 队列句柄
    handlers: HashMap<String, Handler>, // topic → handler 映射
    max_attempts: u32, // 最大投递次数
    backoff_base: Duration, // 重试基础退避
    dead_letter_topic: String, // 死信目标 topic
}

impl TaskWorker {
    async fn run_task(&self, task_id: usize, shutdown: &mut tokio::sync::watch::Receiver<bool>) { // 单个任务的消费循环
        let mut in_flight = tokio::task::JoinSet::new(); // 在途投递任务集合
        loop { // 主循环
            tokio::select! { // 同时等待停机信号与消息批次
                _ = shutdown.changed() => break, // 收到停机信号即退出拉取循环
                batch = self.queue.receive(16) => match batch { // 否则拉取最多 16 条
                    Ok(deliveries) if deliveries.is_empty() => continue, // 空批次直接进入下一轮
                    Ok(deliveries) => { // 拿到非空批次
                        // 串行 for + 退避同步 sleep 会让一条毒消息霸占整个任务
                        //（退避期间不拉新消息、同批其它消息排队）；每条投递独立任务
                        for d in deliveries { // 逐条投递
                            let worker = self.clone(); // 克隆任务侧状态
                            in_flight.spawn(async move { // 为每条投递启动独立任务
                                worker.dispatch(task_id, d).await; // 异步分发处理
                            });
                        }
                    }
                    Err(e) => { // 拉取出错
                        tracing::error!(task = task_id, error = %e, "queue receive failed"); // 记录拉取失败日志
                        tokio::time::sleep(Duration::from_secs(1)).await; // 退避 1s 后重试
                    }
                }
            }
        }
        // 优雅停机：等在途投递（含退避中的重试）结束后再退出
        while in_flight.join_next().await.is_some() {} // 逐个等待在途任务结束
    }

    async fn dispatch(&self, task_id: usize, delivery: Delivery) { // 处理单条投递（含重试与死信）
        let msg = &delivery.message; // 取出消息引用
        let Some(handler) = self.handlers.get(&msg.topic) else { // 查 handler，缺失则走兜底
            // publish 只接受已注册 topic，正常不会走到这里
            tracing::warn!(topic = %msg.topic, id = %msg.id, "no handler, discarding"); // 记录告警
            let _ = self.queue.ack(&delivery).await; // 直接确认丢弃
            return; // 结束处理
        };

        let rounds = self.max_attempts.max(1); // 计算实际最大尝试轮数
        let mut last_err: Option<QueueError> = None; // 记录最后一次错误
        for attempt in 1..=rounds { // 从第 1 次到最大轮数
            let mut m = msg.clone(); // 克隆消息以设置本次尝试次数
            m.attempts = attempt; // 写入当前尝试次数
            match (handler)(m).await { // 调用 handler 处理
                Ok(()) => { // 处理成功
                    let _ = self.queue.ack(&delivery).await; // 确认消费成功
                    return; // 结束处理
                }
                Err(e) => { // 处理失败
                    last_err = Some(e); // 暂存错误
                    if attempt < rounds { // 若还有剩余重试次数
                        let backoff = self.backoff( // 计算本次退避时长
                            attempt, // 传入当前尝试次数
                        );
                        tracing::warn!( // 记录重试告警
                            task = task_id, // 记录任务号
                            topic = %msg.topic, // 记录 topic
                            id = %msg.id, // 记录消息 ID
                            attempt, // 记录当前尝试次数
                            error = %last_err.as_ref().unwrap(), // 记录错误内容
                            "queue handler failed, retrying" // 日志文案
                        );
                        tokio::time::sleep(backoff).await; // 退避等待后重试
                    }
                }
            }
        }

        let err = last_err.unwrap_or(QueueError::Backend("unknown".to_string())); // 取最终错误（兜底 unknown）
        tracing::error!( // 记录永久失败日志
            topic = %msg.topic, // 记录 topic
            id = %msg.id, // 记录消息 ID
            attempts = rounds, // 记录已尝试轮数
            error = %err, // 记录最终错误
            "queue handler failed permanently" // 日志文案
        );
        // 重试耗尽：配置了死信且转发成功 → ack（消息已妥善安置）；
        // 未配置死信（或转发失败）→ 只 nack 交由后端处置（redis 留 pending、
        // rabbitmq DLX/reject、kafka 不提交位移）。nack 后再 ack 在 rabbitmq 上
        // 会因 delivery tag 已结算触发 PRECONDITION_FAILED 毒化整个 channel。
        let mut dead_lettered = false; // 标记是否已成功转发死信
        if !self.dead_letter_topic.is_empty() && self.dead_letter_topic != msg.topic { // 配置了死信且不同于原 topic
            let mut dead = msg.clone(); // 克隆消息作为死信
            dead.topic = self.dead_letter_topic.clone(); // 改投到死信 topic
            dead.attempts = rounds; // 记录已达尝试轮数
            match self // 发布死信
                .queue // 访问队列
                .publish(&dead.topic, dead.payload, dead.headers) // 发布死信消息
                .await // 等待发布完成
            {
                Ok(_) => dead_lettered = true, // 发布成功则标记已转发
                Err(e) => tracing::error!(error = %e, "dead letter publish failed, leaving message unacked"), // 发布失败则记录错误、消息保持未确认
            }
        }
        if dead_lettered { // 若死信已成功转发
            let _ = self.queue.ack(&delivery).await; // 确认原消息（已妥善安置）
        } else { // 否则未配置死信或转发失败
            let _ = self.queue.nack(&delivery).await; // 交由后端处置（不 ack 以免重复确认）
        }
    }

    /// 指数退避 + 抖动：base * 2^(attempt-1)，上限 60s
    fn backoff(&self, attempt: u32) -> Duration { // 计算第 attempt 次重试的退避时长
        let exp = self // 指数退避计算
            .backoff_base // 基础退避时长
            .saturating_mul(1u32.wrapping_shl(attempt.saturating_sub(1)).max(1)); // 乘以 2^(attempt-1)（饱和运算防溢出）
        let capped = exp.min(Duration::from_secs(60)); // 上限封顶 60s
        let jitter = rand_jitter(capped); // 叠加随机抖动
        capped + jitter // 返回封顶值加抖动
    }
}

fn rand_jitter(d: Duration) -> Duration { // 生成 0~9.9% 的随机抖动
    // 轻量抖动：0~9.9% 随机；避免引 rand，时间熵足够
    let nanos = std::time::SystemTime::now() // 取当前系统时间
        .duration_since(std::time::UNIX_EPOCH) // 计算距 UNIX 纪元的时长
        .map(|t| t.subsec_nanos() as u64) // 取其亚秒纳秒部分作为熵
        .unwrap_or(0); // 时间异常时兜底为 0
    Duration::from_nanos(d.as_nanos() as u64 * (nanos % 100) / 100) // 抖动幅度为 0~99/10000 的时长
}

/// Worker 运行句柄：App 持有到进程结束；`shutdown()` 置位后各任务排空退出
pub struct WorkerRunner { // 持有消费任务的运行句柄
    stop_tx: tokio::sync::watch::Sender<bool>, // 停机信号发送端
    tasks: Vec<tokio::task::JoinHandle<()>>, // 各并发任务的句柄
    queue: QueueHandle, // 队列句柄（关闭时使用）
}

impl WorkerRunner {
    /// 优雅停机：停止拉取 → 等待在途任务结束 → 关闭后端
    pub async fn shutdown(self) { // 执行优雅停机
        let _ = self.stop_tx.send(true); // 发送停机信号，通知各任务停止拉取
        for t in self.tasks { // 逐个等待任务结束
            let _ = t.await; // 等待该任务退出
        }
        let _ = self.queue.close().await; // 关闭队列后端
    }

    pub fn backend_name(&self) -> &'static str { // 返回后端名称
        self.queue.name() // 透传队列的后端名
    }
}
