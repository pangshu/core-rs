//! Kafka 队列后端（feature = "queue-kafka"，rdkafka）：topic 名即 Kafka topic，
//! 消费组语义与 redis 后端一致（同组分摊、异组各收一份）。
//! ack = 提交位移（at-least-once）；nack 不提交（重启后重投，配合 Worker 重试）。
//!
//! 构建依赖：rdkafka 需要本机 CMake / C 工具链编译 librdkafka。

use std::collections::{BTreeMap, HashSet}; // 引入有序映射与哈希集合
use std::sync::atomic::{AtomicBool, Ordering}; // 引入原子布尔与内存序
use std::sync::{Arc, Mutex}; // 引入共享指针与互斥锁
use std::time::Duration; // 引入时长类型（发送超时/拉取超时）

use rdkafka::config::ClientConfig; // 引入 Kafka 客户端配置
use rdkafka::consumer::{Consumer, StreamConsumer}; // 引入消费者 trait 与流式消费者
use rdkafka::producer::{FutureProducer, FutureRecord}; // 引入异步生产者与记录类型
use rdkafka::Message as KafkaMessage; // 引入消息 trait（payload_view/topic 等方法）

use super::{Delivery, Queue, QueueError}; // 引入队列公共类型
use crate::config::sections::QueueKafkaSettings; // 引入 Kafka 队列配置节

struct State {
    topics: HashSet<String>, // 已订阅的 topic 集合
}

pub struct KafkaQueue {
    producer: FutureProducer, // 异步生产者
    consumer: Arc<StreamConsumer>, // 流式消费者（共享给 ack 提交位移）
    state: Arc<Mutex<State>>, // 共享可变状态（topic 集合）
    closed: Arc<AtomicBool>, // 关闭标志
}

impl KafkaQueue {
    /// 建连即验证（坏 broker 在装配期报错）
    pub async fn connect(settings: &QueueKafkaSettings) -> Result<Self, QueueError> { // 异步建连并初始化
        let producer: FutureProducer = ClientConfig::new() // 构造生产者配置
            .set("bootstrap.servers", &settings.brokers) // broker 地址列表
            .set("enable.idempotence", "true") // 幂等生产者，避免重试导致重复
            .create()
            .map_err(|e| QueueError::Backend(format!("kafka producer failed: {e}")))?; // 失败即装配期报错
        let consumer: StreamConsumer = ClientConfig::new() // 构造消费者配置
            .set("bootstrap.servers", &settings.brokers) // broker 地址列表
            .set("group.id", &settings.group) // 消费组 id
            .set("enable.auto.commit", "false") // 关闭自动提交，改为手动 ack
            // earliest：位移未提交（首次上线/位移丢失）时从最早消费，避免静默跳过停机窗口
            .set("auto.offset.reset", "earliest") // 无位移时从最早开始
            .create()
            .map_err(|e| QueueError::Backend(format!("kafka consumer failed: {e}")))?; // 失败即装配期报错
        Ok(Self { // 组装实例
            producer, // 生产者
            consumer: Arc::new(consumer), // 消费者包成 Arc 便于 ack 时共享
            state: Arc::new(Mutex::new(State { topics: HashSet::new() })), // 初始化空的 topic 集合
            closed: Arc::new(AtomicBool::new(false)), // 初始未关闭
        })
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> { // 加锁并容忍 poison
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
    }
}

#[async_trait::async_trait] // 为异步 trait 实现提供宏支持
impl Queue for KafkaQueue { // 为 KafkaQueue 实现统一队列接口
    fn name(&self) -> &'static str { // 后端名称
        "kafka" // 固定标识
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> { // 注册 topic 并订阅
        {
            let mut st = KafkaQueue::lock(&self.state); // 加锁登记 topic
            if !st.topics.insert(topic.to_string()) { // 重复注册检测
                return Err(QueueError::AlreadyRegistered(topic.to_string())); // 已注册则报错
            }
        }
        let topics: Vec<&str> = KafkaQueue::lock(&self.state) // 取当前全部 topic
            .topics
            .iter()
            .map(|s| s.as_str()) // 转成 &str 供订阅
            .collect();
        self.consumer // 重新订阅全部 topic
            .subscribe(&topics)
            .map_err(|e| QueueError::Backend(format!("kafka subscribe failed: {e}")))?; // 订阅失败转后端错误
        Ok(())
    }

    async fn publish( // 发布消息到 topic
        &self, // 自身引用
        topic: &str, // 目标 topic
        payload: serde_json::Value, // 消息体
        headers: BTreeMap<String, String>, // 消息头
    ) -> Result<String, QueueError> { // 返回位移字符串
        if !KafkaQueue::lock(&self.state).topics.contains(topic) { // 未注册的 topic 不允许发布
            return Err(QueueError::NoHandler(topic.to_string())); // 返回无处理器错误
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }).to_string(); // 打包为 JSON 字符串
        let record = FutureRecord::to(topic).payload(&body).key(""); // 构造记录（空 key）
        let (offset, _err) = self // 发送并等待结果
            .producer
            .send(record, Duration::from_secs(10)) // 最多等待 10 秒
            .await
            .map_err(|(e, _)| QueueError::Backend(format!("kafka publish failed: {e}")))?; // 失败转后端错误
        Ok(offset.to_string()) // 返回 broker 分配的位移
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> { // 拉取一批消息
        if self.closed.load(Ordering::SeqCst) { // 已关闭则不再拉取
            return Ok(Vec::new()); // 返回空批
        }
        let mut out = Vec::new(); // 结果收集
        while out.len() < max.max(1) { // 未达本批上限则持续取
            // 首条等待 1s，后续立即取积压
            let timeout = if out.is_empty() { // 首条用较长超时等待
                Duration::from_secs(1)
            } else { // 已有消息则快速取积压
                Duration::from_millis(1)
            };
            match tokio::time::timeout(timeout, self.consumer.recv()).await { // 带超时地接收一条
                Ok(Ok(msg)) => { // 成功收到消息
                    let payload = match msg.payload_view::<str>() { // 取消息体（UTF-8）
                        Some(Ok(s)) => s.to_string(), // 转 String
                        _ => continue, // 非 UTF-8 则跳过
                    };
                    let topic = msg.topic().to_string(); // 取来源 topic
                    if let Some(m) = decode_message(&payload, &topic) { // 解析消息体
                        // ack_token：partition:offset（ack 时提交）
                        let token = format!("{}:{}", msg.partition(), msg.offset()); // 拼接分区与位移
                        out.push(Delivery { // 组装投递体
                            ack_token: token, // ack 令牌
                            message: m, // 解析后的消息
                        });
                    }
                }
                Ok(Err(e)) => { // 接收出错
                    tracing::warn!(error = %e, "kafka receive failed"); // 记警告
                    break; // 结束本轮
                }
                Err(_) => break, // 等待超时
            }
        }
        Ok(out) // 返回本批结果
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 确认消息：提交位移
        // 提交位移到 broker（at-least-once）。只 store_offset 不 commit 的话，
        // 位移从不持久化，重启后按 auto.offset.reset 重新开始 = 停机窗口消息全丢。
        // Async 提交：进程崩溃最多重复投递（不丢），符合 at-least-once 语义。
        let (partition, offset) = delivery // 拆出分区与位移
            .ack_token
            .split_once(':') // ack_token 形如 partition:offset
            .ok_or_else(|| QueueError::Backend("bad kafka ack token".to_string()))?; // 格式错误转后端错误
        let partition: i32 = partition // 解析分区号
            .parse()
            .map_err(|_| QueueError::Backend("bad kafka partition".to_string()))?; // 解析失败转后端错误
        let offset: i64 = offset // 解析位移
            .parse()
            .map_err(|_| QueueError::Backend("bad kafka offset".to_string()))?; // 解析失败转后端错误
        let mut tpl = rdkafka::topic_partition_list::TopicPartitionList::new(); // 构造分区位移列表
        tpl.add_partition_offset( // 加入「下一待消费位移」
            &delivery.message.topic, // 目标 topic
            partition, // 分区号
            rdkafka::topic_partition_list::Offset::Offset(offset + 1), // 提交 offset+1（即已消费到当前条）
        )
        .map_err(|e| QueueError::Backend(format!("kafka offset build failed: {e}")))?; // 构建失败转后端错误
        self.consumer // 异步提交位移
            .commit(&tpl, rdkafka::consumer::CommitMode::Async)
            .map_err(|e| QueueError::Backend(format!("kafka commit failed: {e}")))?; // 提交失败转后端错误
        Ok(())
    }

    async fn nack(&self, _delivery: &Delivery) -> Result<(), QueueError> { // 否定确认：不提交位移
        // 不提交位移：重启/重平衡后重投；Worker 重试语义不受影响
        Ok(()) // 空实现，位移保持未提交
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
