//! 实时通信（feature `ws` / `sse`）。
//!
//! - [`Hub`]：频道广播中心（`tokio::sync::broadcast`），并负责**订阅授权**、
//!   **连接数治理**（全局 + 每 IP + 来源不可知时的共用配额）、**频道表生命周期**
//!   （订阅者归零即摘除、`max_topics` 上限）与**优雅关闭**
//! - [`RealtimeMessage`]：广播消息
//! - [`websocket::handler`]（`ws`）/ [`sse::stream`]（`sse`）：两种端点共用同一 `Hub`
//!
//! **建连顺序**（WS / SSE 一致）：`Origin 白名单 → 频道名校验 → 订阅授权 →
//! 连接数上限 → 订阅`。前四步都在升级 / 建流**之前**，失败不消耗连接名额。
//!
//! **订阅授权是 fail-closed**：未调用 [`Hub::on_subscribe`] 注册钩子时端点一律拒绝。
//! 频道名通常是 `user:123` 这类可猜的业务 ID，只校验身份不校验归属等于公开别人的频道。
//!
//! 连接治理要点（见 `docs/03` §4.20）：升级前校验连接数、单帧/单消息大小上限
//! （**入站与出站都受约束**）、上行速率上限、心跳超时、出站写超时断开；关闭时
//! WS 发 `Close(1001)`、SSE 结束流。
//!
//! **不做**：不做消息持久化（由 `queue/` 承担）。
//!
//! **上行消息**：客户端发的 `Text` / `Binary` 帧会构造 [`InboundMessage`]，交给
//! [`Hub::on_message`] 注册的处理器（未注册时忽略，仅更新心跳）。
//!
//! **跨实例转发**：`[realtime].forward_via_queue = true` + `queue` + `cache` 时，
//! [`Hub::with_forwarding`] 会启动实例注册表心跳与转发消费者——每个实例消费自己的
//! 队列，生产者按注册表扇出（队列是点对点语义，不能大家抢同一个队列）。
//!
//! 文件划分：`hub`（广播与连接治理）/ `topic`（频道名校验）/ `message`（消息类型）/
//! `websocket`·`sse`（端点）/ `routes`（统一入口）/ `forward`（跨实例转发）。

// 跨实例转发同时需要 queue（传输）与 cache（实例注册表），故两个 feature 都要开
#[cfg(all(feature = "queue", feature = "cache"))]
pub mod forward;
pub mod hub;
pub mod message;
mod routes;
#[cfg(feature = "sse")]
pub mod sse;
pub mod topic;
#[cfg(feature = "ws")]
pub mod websocket;

pub use hub::{
    ConnectionGuard, Hub, InboundHandler, SubscribeAuthorizer, SubscribeRequest, Subscription,
    TOPIC_BUFFER, subject_from_extensions,
};
pub use message::{InboundMessage, MessageKind, RealtimeMessage};
pub use routes::handle;
