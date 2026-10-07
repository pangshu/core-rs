//! 进程内内存队列（feature = "queue-memory"，默认）：全实例一个有界 channel，
//! `receive` 直接从中拉取（Worker 的并发任务天然分摊）。失败重试与死信由
//! Worker 统一承担；重启即丢，适合开发环境与可容忍丢失的轻量任务。

use std::collections::{BTreeMap, HashSet}; // 引入有序 map 与集合：头部键值与已注册 topic 集合
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering}; // 引入原子布尔/原子计数与内存序
use std::sync::{Arc, Mutex}; // 引入 Arc（共享关闭标志）与标准互斥锁

use tokio::sync::mpsc; // 引入 tokio 有界异步通道

use super::{Delivery, Queue, QueueError}; // 引入队列契约与投递/错误类型
use crate::config::sections::QueueSettings; // 引入 `[queue]` 配置段类型

struct State { // 受互斥锁保护的队列内部状态
    topics: HashSet<String>, // 已注册的 topic 集合
    closed: bool, // 是否已关闭
}

pub struct MemoryQueue { // 进程内内存队列实现
    /// close 时置 None（drop sender，消费端 recv 返回 None 自然排空退出）
    tx: Mutex<Option<mpsc::Sender<crate::queue::Message>>>, // 发送端，关闭时置 None 以释放
    /// tokio 异步锁：recv 阻塞期间不阻塞其他任务（多 Worker 轮流持锁等待）
    rx: tokio::sync::Mutex<Option<mpsc::Receiver<crate::queue::Message>>>, // 接收端，异步锁保护
    state: Mutex<State>, // 受锁保护的注册/关闭状态
    seq: AtomicU64, // 进程内消息自增序号
    closed: Arc<AtomicBool>, // 无锁关闭标志（供跨任务快速判断）
}

impl MemoryQueue {
    pub fn new(settings: &QueueSettings) -> Self { // 按配置构造内存队列
        let (tx, rx) = mpsc::channel(settings.memory.buffer.max(1)); // 创建至少容量 1 的有界通道
        Self { // 组装队列实例
            tx: Mutex::new(Some(tx)), // 发送端包入锁与 Option
            rx: tokio::sync::Mutex::new(Some(rx)), // 接收端包入异步锁与 Option
            state: Mutex::new(State { // 初始化受锁状态
                topics: HashSet::new(), // 初始无已注册 topic
                closed: false, // 初始未关闭
            }),
            seq: AtomicU64::new(0), // 序号从 0 起
            closed: Arc::new(AtomicBool::new(false)), // 关闭标志初始为 false
        }
    }

    fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> { // 获取状态锁的辅助方法
        state.lock().unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
    }
}

#[async_trait::async_trait] // 用宏把 async trait 实现降级为可对象安全形态
impl Queue for MemoryQueue { // 为内存队列实现统一队列契约
    fn name(&self) -> &'static str { // 返回后端名称
        "memory" // 后端名为 memory
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> { // 注册一个 topic
        let mut st = Self::lock(&self.state); // 加锁取得可变状态
        if st.closed { // 若队列已关闭
            return Err(QueueError::Closed); // 返回已关闭错误
        }
        if !st.topics.insert(topic.to_string()) { // 插入集合；已存在则返回 false
            return Err(QueueError::AlreadyRegistered(topic.to_string())); // 重复注册返回错误
        }
        Ok(()) // 注册成功
    }

    async fn publish( // 发布一条消息
        &self, // 自身引用
        topic: &str, // 目标 topic
        payload: serde_json::Value, // JSON 载荷
        headers: BTreeMap<String, String>, // 透传头
    ) -> Result<String, QueueError> { // 成功返回消息 ID
        {
            let st = Self::lock(&self.state); // 短暂加锁校验
            if st.closed { // 若队列已关闭
                return Err(QueueError::Closed); // 返回已关闭错误
            }
            if !st.topics.contains(topic) { // 若 topic 未注册
                return Err(QueueError::NoHandler(topic.to_string())); // 返回无处理器错误
            }
        }
        let mut msg = crate::queue::Message::new(topic, payload); // 构造消息
        msg.headers = headers; // 填入透传头
        msg.id = self.seq.fetch_add(1, Ordering::Relaxed).to_string(); // 用自增序号覆盖消息 ID
        let tx = self // 读取发送端克隆
            .tx // 访问发送端字段
            .lock() // 加锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // poison 时取出内部值
            .clone(); // 克隆 Sender（Sender 可克隆共享）
        let Some(tx) = tx else { // 若发送端已置 None（已关闭）
            return Err(QueueError::Closed); // 返回已关闭错误
        };
        // 队满报错而非阻塞业务（与 go-admin-core memory 队列语义一致）
        let id = msg.id.clone(); // 提前保存消息 ID 供返回
        tx.try_send(msg) // 非阻塞投递
            .map_err(|_| QueueError::Full(topic.to_string()))?; // 队满则转为 Full 错误
        Ok(id) // 返回消息 ID
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> { // 拉取一批消息
        let mut guard = self.rx.lock().await; // 异步加锁取得接收端
        let Some(rx) = guard.as_mut() else { // 若接收端已置 None
            return Ok(Vec::new()); // 已关闭（close 后 sender drop，recv 返回 None 排空退出）
        };
        let mut out = Vec::new(); // 本轮结果集
        for _ in 0..max.max(1) { // 至少尝试一次，最多 max 条
            // 首条阻塞等待（最多 500ms，避免长期占用锁）；拿到后改 try_recv 排空积压
            let msg = if out.is_empty() { // 本批首条走阻塞等待
                match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await { // 带 500ms 超时等待
                    Ok(m) => m, // 正常拿到消息
                    Err(_) => break, // 等待超时，本轮无消息
                }
            } else { // 非首条：改为非阻塞排空
                rx.try_recv().ok() // 非阻塞尝试取一条，空则得 None
            };
            let Some(msg) = msg else { // 若未取到消息
                // channel 关闭：置空 receiver，后续 receive 直接返回空
                *guard = None; // 置空接收端标记已关闭
                break; // 结束本轮
            };
            out.push(Delivery { // 组装投递结果
                ack_token: String::new(), // 内存后端无确认凭据，留空
                message: msg, // 放入实际消息
            });
        }
        Ok(out) // 返回本轮拉取结果
    }

    async fn ack(&self, _delivery: &Delivery) -> Result<(), QueueError> { // 确认消费成功
        Ok(()) // 拉取即消费，无需确认
    }

    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 确认消费失败
        // 重试语义由 Worker 承担；到达 nack 意味着重试耗尽，只能记日志丢弃
        tracing::error!( // 记录一条错误日志表示消息被丢弃
            topic = %delivery.message.topic, // 记录 topic
            id = %delivery.message.id, // 记录消息 ID
            "memory queue message dropped after retries" // 日志文案
        );
        Ok(()) // 内存后端不重试，直接返回成功
    }

    async fn close(&self) -> Result<(), QueueError> { // 幂等关闭队列
        let mut st = Self::lock(&self.state); // 加锁取得状态
        if st.closed { // 若已关闭
            return Ok(()); // 幂等：直接返回成功
        }
        st.closed = true; // 标记状态为已关闭
        st.topics.clear(); // 清空已注册 topic
        self.closed.store(true, Ordering::SeqCst); // 置位无锁关闭标志
        // drop 全部 sender：消费者排空缓冲后自然退出（优雅排空）
        *self.tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None; // 置空发送端触发排空退出
        Ok(()) // 关闭成功
    }
}
