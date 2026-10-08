//! 队列契约：[`Queue`] trait、投递结构 [`Delivery`] 与共享句柄 [`QueueHandle`]。
//!
//! 拉取式设计，天然适配 memory / redis stream / rabbitmq / kafka / nats；
//! 任务分发（一条消息一个 worker）与事件广播（异消费组各收一份）共用同一抽象。

use std::collections::BTreeMap; // 引入有序映射，用于消息 headers 的透传键值

use super::Message; // 引入消息结构（queue/mod.rs 已重导出）
use super::QueueError; // 引入队列统一错误类型

/// 已投递、待确认的消息（`ack_token` 为后端私有确认凭据，业务侧不感知）
#[derive(Debug, Clone)] // 派生 Debug 与 Clone，便于日志打印与跨任务克隆
pub struct Delivery { // 定义一次投递结果结构
    pub message: Message, // 实际消息内容
    pub ack_token: String, // 后端私有确认凭据（业务不感知）
}

/// 队列契约（拉取式，天然适配 memory / redis stream / rabbitmq / kafka / nats）。
#[async_trait::async_trait] // 用 async_trait 宏把 async trait 方法降级为可对象安全的形态
pub trait Queue: Send + Sync { // 定义队列后端统一接口，要求可跨线程共享
    /// 后端名（日志用）
    fn name(&self) -> &'static str; // 返回后端名称，用于日志与诊断

    /// 注册 topic（必须在消费启动前；redis = 建消费组，rabbitmq = 声明队列）。
    /// 重复注册报 [`QueueError::AlreadyRegistered`]。
    async fn register(&self, topic: &str) -> Result<(), QueueError>; // 注册一个 topic，须在消费启动前调用

    /// 发布消息到 topic（未注册报 [`QueueError::NoHandler`]），返回消息 ID
    async fn publish( // 发布消息到指定 topic
        &self, // 自身引用
        topic: &str, // 目标 topic 名
        payload: serde_json::Value, // JSON 载荷
        headers: BTreeMap<String, String>, // 透传头键值对
    ) -> Result<String, QueueError>; // 成功返回分配的消息 ID

    /// 拉取一批消息（内部阻塞至拿到消息或 block 超时；空 Vec = 本轮无消息）。
    /// 只包含已注册的 topic。
    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError>; // 拉取最多 max 条待确认消息

    /// 确认消费成功
    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError>; // 确认某条消息消费成功

    /// 确认消费失败（后端自行决定：redis 留 pending 待接管，rabbitmq reject，
    /// memory 记日志丢弃——重试语义统一由 Worker 承担）
    async fn nack(&self, delivery: &Delivery) -> Result<(), QueueError>; // 确认消费失败，交由后端处置

    /// 优雅关闭（幂等）：memory 排空在途消息后退出；redis/rabbitmq 不排空，
    /// 未 ACK 消息由其他实例接管（at-least-once，消费方须幂等）
    async fn close(&self) -> Result<(), QueueError>; // 幂等关闭队列，触发优雅停机

    /// 存活探测（/ready 用）
    async fn ping(&self) -> Result<(), QueueError> { // 默认存活探测实现（后端可覆盖）
        Ok(()) // 默认视为存活，返回成功
    }
}

/// 共享队列句柄（存于 CoreState）
pub type QueueHandle = std::sync::Arc<dyn Queue>; // 用 Arc 包裹 trait 对象，便于跨线程共享
