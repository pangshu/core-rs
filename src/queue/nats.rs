//! NATS 队列后端（feature = "queue-nats"，async-nats + JetStream）：
//! stream/durable consumer 固定（配置节声明），topic 即 subject filter。
//! ack = message.ack()；nack = message.nak()（服务端按消费策略重投）。

use std::collections::{BTreeMap, HashSet}; // 引入有序映射与哈希集合
use std::sync::atomic::{AtomicBool, Ordering}; // 引入原子布尔与内存序
use std::sync::{Arc, Mutex}; // 引入共享指针与互斥锁（同步锁保护 topic 集合）
use std::time::Duration; // 引入时长类型（拉取超时/Nak 延迟）

use async_nats::jetstream::{self, consumer::PullConsumer}; // 引入 JetStream 上下文与拉取消费者
use async_nats::Client; // 引入 NATS 客户端
use futures::StreamExt as _; // 引入 Stream 扩展（messages.next()）

use super::{Delivery, Queue, QueueError}; // 引入队列公共类型
use crate::config::sections::QueueNatsSettings; // 引入 NATS 队列配置节

struct State {
    topics: HashSet<String>, // 已注册的 topic 集合
}

pub struct NatsQueue {
    client: Client, // NATS 客户端
    js: jetstream::Context, // JetStream 上下文（发布用）
    consumer: tokio::sync::Mutex<PullConsumer>, // 拉取消费者（异步锁保护）
    /// 已投递未确认的消息（ack_token → 消息对象；ack/nack 需要原始对象）
    pending: tokio::sync::Mutex<std::collections::HashMap<String, jetstream::Message>>, // 待确认消息表
    state: Arc<Mutex<State>>, // 共享可变状态（topic 集合）
    closed: Arc<AtomicBool>, // 关闭标志
}

impl NatsQueue {
    /// 建连即验证（坏地址在装配期报错）
    pub async fn connect(settings: &QueueNatsSettings) -> Result<Self, QueueError> { // 异步建连并初始化
        let client = async_nats::connect(settings.url.as_str()) // 连接 NATS 服务器
            .await
            .map_err(|e| QueueError::Backend(format!("nats connect failed: {e}")))?; // 失败即装配期报错
        let js = jetstream::new(client.clone()); // 基于客户端创建 JetStream 上下文

        // stream：subjects 全收（ *> ），按 topic 过滤在 consumer 上做
        let stream = js // 获取或创建 stream
            .get_or_create_stream(jetstream::stream::Config { // stream 配置
                name: settings.stream.clone(), // stream 名（配置声明）
                subjects: vec!["core-rs.>".to_string()], // 收全部 core-rs. 前缀 subject
                ..Default::default() // 其余选项取默认
            })
            .await
            .map_err(|e| QueueError::Backend(format!("nats stream failed: {e}")))?; // 失败即装配期报错
        // durable pull consumer（空 filter：receive 侧按已注册 subject 分发）；
        // max_deliver 必须设置：默认无限重投会让毒消息永久循环
        let consumer = stream // 获取或创建 durable 消费者
            .get_or_create_consumer(
                &settings.durable, // durable 名（配置声明）
                jetstream::consumer::pull::Config { // 拉取消费者配置
                    durable_name: Some(settings.durable.clone()), // 持久化消费者名
                    filter_subject: "core-rs.>".to_string(), // 订阅过滤：全收
                    max_deliver: settings.max_deliver.max(1), // 最大投递次数至少为 1
                    ..Default::default() // 其余选项取默认
                },
            )
            .await
            .map_err(|e| QueueError::Backend(format!("nats consumer failed: {e}")))?; // 失败即装配期报错

        Ok(Self { // 组装实例
            client, // NATS 客户端
            js, // JetStream 上下文
            consumer: tokio::sync::Mutex::new(consumer), // 拉取消费者加异步锁
            pending: tokio::sync::Mutex::new(std::collections::HashMap::new()), // 初始无待确认消息
            state: Arc::new(Mutex::new(State { topics: HashSet::new() })), // 初始化空的 topic 集合
            closed: Arc::new(AtomicBool::new(false)), // 初始未关闭
        })
    }

    fn subject(topic: &str) -> String { // 由 topic 拼出 NATS subject
        // NATS subject 不允许 '/' 等字符：统一前缀 + '.' 分隔
        format!("core-rs.{topic}") // 固定前缀 + topic
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> { // 加锁并容忍 poison
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
    }
}

#[async_trait::async_trait] // 为异步 trait 实现提供宏支持
impl Queue for NatsQueue { // 为 NatsQueue 实现统一队列接口
    fn name(&self) -> &'static str { // 后端名称
        "nats" // 固定标识
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> { // 注册 topic
        if !NatsQueue::lock(&self.state).topics.insert(topic.to_string()) { // 重复注册检测
            return Err(QueueError::AlreadyRegistered(topic.to_string())); // 已注册则报错
        }
        Ok(())
    }

    async fn publish( // 发布消息到 subject
        &self, // 自身引用
        topic: &str, // 目标 topic
        payload: serde_json::Value, // 消息体
        headers: BTreeMap<String, String>, // 消息头
    ) -> Result<String, QueueError> { // 返回 stream 序列号
        if !NatsQueue::lock(&self.state).topics.contains(topic) { // 未注册的 topic 不允许发布
            return Err(QueueError::NoHandler(topic.to_string())); // 返回无处理器错误
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }).to_string(); // 打包为 JSON 字符串
        let ack = self // 发布并拿到确认句柄
            .js
            .publish(Self::subject(topic), body.into_bytes().into()) // 发布到对应 subject
            .await
            .map_err(|e| QueueError::Backend(format!("nats publish failed: {e}")))?; // 失败转后端错误
        let ack = ack // 等待服务端确认
            .await
            .map_err(|e| QueueError::Backend(format!("nats publish ack failed: {e}")))?; // 确认失败转后端错误
        Ok(ack.sequence.to_string()) // 返回 stream 序列号作为消息 id
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> { // 拉取一批消息
        if self.closed.load(Ordering::SeqCst) { // 已关闭则不再拉取
            return Ok(Vec::new()); // 返回空批
        }
        let consumer = self.consumer.lock().await; // 取得消费者（持锁至本轮结束）
        let batch = max.clamp(1, 100); // 本批条数限制在 1~100
        let mut messages = consumer // 发起批量拉取
            .batch()
            .max_messages(batch) // 最多 batch 条
            .expires(Duration::from_millis(500)) // 500ms 内无消息即结束
            .messages()
            .await
            .map_err(|e| QueueError::Backend(format!("nats fetch failed: {e}")))?; // 失败转后端错误
        let mut out = Vec::new(); // 结果收集
        while let Some(msg) = messages.next().await { // 逐条消费拉取结果
            let msg = match msg { // 单条结果可能是错误
                Ok(m) => m, // 正常消息
                Err(e) => { // 拉取错误
                    tracing::warn!(error = %e, "nats message error"); // 记警告
                    continue; // 跳过该条
                }
            };
            // subject "core-rs.<topic>" → 还原 topic（'.' 分隔的原 topic 保留原样首个段）
            let subject = msg.subject.as_str(); // 取消息 subject
            let topic = subject // 去掉固定前缀还原 topic
                .strip_prefix("core-rs.") // 剥离前缀
                .unwrap_or(subject) // 无前缀则原样使用
                .to_string(); // 转 String
            let body = String::from_utf8_lossy(&msg.message.payload).to_string(); // 消息体字节转字符串
            if let Some(m) = decode_message(&body, &topic) { // 解析消息体
                let token = msg // 取 stream 序列号作为 ack 令牌
                    .info()
                    .ok()
                    .map(|i| i.stream_sequence.to_string()) // 序列号转字符串
                    .unwrap_or_default(); // 取不到则为空
                self.pending.lock().await.insert(token.clone(), msg.clone()); // 暂存原始消息供 ack/nack
                out.push(Delivery { // 组装投递体
                    ack_token: token, // ack 令牌
                    message: m, // 解析后的消息
                });
            }
            if out.len() >= batch { // 达到本批上限
                break; // 停止消费
            }
        }
        Ok(out) // 返回本批结果
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 确认消息已处理
        if let Some(msg) = self.pending.lock().await.remove(&delivery.ack_token) { // 取出并移除待确认消息
            msg.ack() // 向服务端确认
                .await
                .map_err(|e| QueueError::Backend(format!("nats ack failed: {e}")))?; // 失败转后端错误
        }
        Ok(())
    }

    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 否定确认：请求重投
        if let Some(msg) = self.pending.lock().await.remove(&delivery.ack_token) { // 取出并移除待确认消息
            // Nak 附带延迟：立即重投会让失败消息在窗口内高频打转
            msg.ack_with(async_nats::jetstream::AckKind::Nak(Some( // 带延迟的 Nak
                Duration::from_secs(2), // 延迟 2 秒再重投
            )))
            .await
            .map_err(|e| QueueError::Backend(format!("nats nak failed: {e}")))?; // 失败转后端错误
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), QueueError> { // 关闭后端
        self.closed.store(true, Ordering::SeqCst); // 置关闭标志，receive 立即返回空
        Ok(())
    }
}

fn decode_message(body: &str, topic: &str) -> Option<crate::queue::Message> { // 反序列化消息体
    #[derive(serde::Deserialize)] // 派生反序列化实现
    struct Body { // 消息体结构（与 publish 打包格式对应）
        payload: serde_json::Value, // 业务载荷
        #[serde(default)] // headers 缺失时用默认空表
        headers: BTreeMap<String, String>, // 消息头
    }
    let parsed: Body = serde_json::from_str(body).ok()?; // 解析 JSON，失败返回 None
    let mut msg = crate::queue::Message::new(topic, parsed.payload); // 构造框架消息对象
    msg.headers = parsed.headers; // 回填消息头
    Some(msg) // 返回消息
}

#[allow(dead_code)] // 允许未使用（保留客户端引用，避免连接被提前释放）
fn _client_alive(q: &NatsQueue) -> &Client { // 持有客户端引用的辅助函数
    &q.client // 返回客户端引用
}
