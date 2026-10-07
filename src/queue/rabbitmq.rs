//! RabbitMQ 队列后端（feature = "queue-rabbitmq"，lapin）：topic 名即队列名，
//! 经默认交换机直投（routing_key = topic）。注册 = durable 队列声明；
//! receive = basic.get 轮询；ack/nack = basic.ack / basic.reject。
//! 重试语义统一由 Worker 承担；nack reject 不重回队列（可配 DLX 的部署进死信）。

use std::collections::{BTreeMap, HashSet}; // 引入有序映射与哈希集合
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering}; // 引入原子布尔/计数与内存序
use std::sync::{Arc, Mutex}; // 引入共享指针与互斥锁

use lapin::options::{BasicAckOptions, BasicGetOptions, BasicPublishOptions, BasicQosOptions, BasicRejectOptions, ConfirmSelectOptions, QueueDeclareOptions}; // 引入 lapin 各类命令选项
use lapin::types::FieldTable; // 引入 AMQP 字段表（空表占位用）
use lapin::BasicProperties; // 引入消息属性（设置持久化等）
use lapin::{Channel, Connection, ConnectionProperties}; // 引入连接、信道与连接属性

use super::{Delivery, Queue, QueueError}; // 引入队列公共类型
use crate::config::sections::QueueRabbitmqSettings; // 引入 RabbitMQ 队列配置节

struct State {
    topics: HashSet<String>, // 已声明的队列名集合
}

pub struct RabbitmqQueue {
    channel: Channel, // 复用的 AMQP 信道
    state: Arc<Mutex<State>>, // 共享可变状态（队列集合）
    closed: Arc<AtomicBool>, // 关闭标志
    seq: AtomicU64, // 自增序号，用于返回发布 id
}

impl RabbitmqQueue {
    /// 建连即验证（坏地址在装配期报错）
    pub async fn connect(settings: &QueueRabbitmqSettings) -> Result<Self, QueueError> { // 异步建连并初始化
        let conn = Connection::connect( // 建立 AMQP 连接
            settings.url.as_str(), // 连接 URL
            ConnectionProperties::default(), // 默认连接属性
        )
        .await // 等待连接建立
        .map_err(|e| QueueError::Backend(format!("rabbitmq connect failed: {e}")))?; // 失败即装配期报错
        let channel = conn // 打开一个信道
            .create_channel()
            .await // 等待信道创建
            .map_err(|e| QueueError::Backend(format!("rabbitmq channel failed: {e}")))?; // 失败转后端错误
        // publisher confirm：publish 的返回确认必须等到 broker ack，
        // 否则 broker 重启/路由失败时消息静默消失
        channel // 开启发布确认模式
            .confirm_select(ConfirmSelectOptions::default())
            .await // 等待确认模式生效
            .map_err(|e| QueueError::Backend(format!("rabbitmq confirm mode failed: {e}")))?; // 失败转后端错误
        channel // 设置预取窗口（QoS）
            .basic_qos(settings.prefetch.max(1), BasicQosOptions::default()) // 每消费者最多未确认 prefetch 条
            .await // 等待设置生效
            .map_err(|e| QueueError::Backend(format!("rabbitmq qos failed: {e}")))?; // 失败转后端错误
        Ok(Self { // 组装实例
            channel, // 已配置好的信道
            state: Arc::new(Mutex::new(State { topics: HashSet::new() })), // 初始化空的队列集合
            closed: Arc::new(AtomicBool::new(false)), // 初始未关闭
            seq: AtomicU64::new(0), // 序号从 0 开始
        })
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> { // 加锁并容忍 poison
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
    }
}

#[async_trait::async_trait] // 为异步 trait 实现提供宏支持
impl Queue for RabbitmqQueue { // 为 RabbitmqQueue 实现统一队列接口
    fn name(&self) -> &'static str { // 后端名称
        "rabbitmq" // 固定标识
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> { // 声明并注册队列
        // durable 队列，topic 名即队列名
        self.channel // 通过信道声明队列
            .queue_declare( // 声明队列命令
                topic, // 队列名即 topic
                QueueDeclareOptions { // 队列声明选项
                    durable: true, // 持久化队列（broker 重启后仍在）
                    ..Default::default() // 其余选项取默认
                },
                FieldTable::default(), // 无附加参数
            )
            .await // 等待声明完成
            .map_err(|e| QueueError::Backend(format!("queue declare failed: {e}")))?; // 失败转后端错误
        if !RabbitmqQueue::lock(&self.state).topics.insert(topic.to_string()) { // 重复注册检测
            return Err(QueueError::AlreadyRegistered(topic.to_string())); // 已注册则报错
        }
        Ok(())
    }

    async fn publish( // 发布消息到队列
        &self, // 自身引用
        topic: &str, // 队列名
        payload: serde_json::Value, // 消息体
        headers: BTreeMap<String, String>, // 消息头
    ) -> Result<String, QueueError> { // 返回自增 id
        if !RabbitmqQueue::lock(&self.state).topics.contains(topic) { // 未注册的队列不允许发布
            return Err(QueueError::NoHandler(topic.to_string())); // 返回无处理器错误
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }).to_string(); // 打包为 JSON 字符串
        let confirm = self // 发布并获取确认句柄
            .channel
            .basic_publish( // 发布命令
                "", // 默认交换机：routing_key 即队列名
                topic, // routing key 用 topic
                BasicPublishOptions { // 发布选项
                    // mandatory：队列不存在时不可无声丢弃，让 confirm 报错
                    mandatory: true, // 路由不到队列时返回错误
                    ..Default::default() // 其余选项取默认
                },
                body.as_bytes(), // 消息体字节
                // delivery_mode=2（持久化）：队列 durable 但消息瞬时的话，
                // broker 重启后队列还在、里面空了
                BasicProperties::default().with_delivery_mode(2), // 标记消息持久化
            )
            .await // 等待发布
            .map_err(|e| QueueError::Backend(format!("publish failed: {e}")))?; // 失败转后端错误
        confirm // 等待 broker 确认
            .await
            .map_err(|e| QueueError::Backend(format!("publish confirm failed: {e}")))?; // 确认失败转后端错误
        Ok(self.seq.fetch_add(1, Ordering::Relaxed).to_string()) // 返回自增序号作为消息 id
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> { // 轮询拉取一批消息
        if self.closed.load(Ordering::SeqCst) { // 已关闭则不再拉取
            return Ok(Vec::new()); // 返回空批
        }
        let topics: Vec<String> = RabbitmqQueue::lock(&self.state).topics.iter().cloned().collect(); // 快照当前队列名
        let mut out = Vec::new(); // 结果收集
        // basic.get 轮询各队列（简单可靠；高吞吐部署建议换 consume + acker 模式）
        for topic in &topics { // 逐个队列轮询
            let Ok(get) = self // 尝试拉取一条消息
                .channel
                .basic_get(topic, BasicGetOptions::default()) // 非阻塞取一条
                .await
            else { // 拉取出错
                continue; // 跳过该队列
            };
            let Some(get) = get else { continue }; // 队列为空则取下一个
            let body = String::from_utf8_lossy(get.data.as_slice()).to_string(); // 消息体字节转字符串
            if let Some(msg) = decode_message(&body, topic) { // 解析消息体
                out.push(Delivery { // 组装投递体
                    ack_token: get.delivery_tag.to_string(), // ack 令牌为投递标签
                    message: msg, // 解析后的消息
                });
            } else { // 解析失败（消息损坏）
                let _ = get.acker.ack(BasicAckOptions::default()).await; // 直接 ack 丢弃，避免毒消息堆积
            }
            if out.len() >= max.max(1) { // 达到本批上限
                break; // 停止轮询
            }
        }
        if out.is_empty() { // 本轮无消息
            // 无消息：小睡后返回空（Worker 循环会重试）
            tokio::time::sleep(std::time::Duration::from_millis(500)).await; // 休眠 500ms 降低空转
        }
        Ok(out) // 返回本批结果
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 确认消息已处理
        let tag: u64 = delivery // 解析投递标签
            .ack_token
            .parse()
            .map_err(|_| QueueError::Backend("bad delivery tag".to_string()))?; // 解析失败转后端错误
        self.channel // 发送 ack
            .basic_ack(tag, BasicAckOptions::default())
            .await
            .map_err(|e| QueueError::Backend(format!("ack failed: {e}")))?; // 失败转后端错误
        Ok(())
    }

    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 否定确认：拒绝且不重回队列
        let tag: u64 = delivery // 解析投递标签
            .ack_token
            .parse()
            .map_err(|_| QueueError::Backend("bad delivery tag".to_string()))?; // 解析失败转后端错误
        self.channel // 发送 reject
            .basic_reject(tag, BasicRejectOptions { requeue: false }) // requeue=false：不重回队列（可进 DLX）
            .await
            .map_err(|e| QueueError::Backend(format!("reject failed: {e}")))?; // 失败转后端错误
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
