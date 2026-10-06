//! 实时通信（文档 三·15）：WebSocket / SSE 统一升级入口与抽象。
//!
//! - [`websocket`]（feature = "ws"）：axum ws 升级、心跳 ping/pong、房间/频道、断线清理；
//! - [`sse`]（feature = "sse"）：Server-Sent Events 单向推送（通知/进度/feed）；
//! - [`hub`]：连接注册表与频道广播；**单机直投，多实例经 Redis Pub/Sub 转发**
//!   （v1，[`forward`]；`[realtime].forward = "queue"` + cache 后端 redis）；
//!   经 queue 消费组转发因"同组分摊"语义不适合广播，列为演进项；
//! - [`message`]：实时消息结构（event / topic / payload），与 `queue::Message`
//!   保持一致的序列化约定。

pub mod hub;
pub mod message;

#[cfg(feature = "cache-redis")]
pub mod forward;

#[cfg(feature = "ws")]
pub mod websocket;
#[cfg(feature = "sse")]
pub mod sse;

pub use hub::Hub;
pub use message::RealtimeMessage;
