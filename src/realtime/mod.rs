//! 实时通信（文档 三·15）：WebSocket / SSE 统一升级入口与抽象。
//!
//! - [`websocket`]（feature = "ws"）：axum ws 升级、心跳 ping/pong、房间/频道、断线清理；
//! - [`sse`]（feature = "sse"）：Server-Sent Events 单向推送（通知/进度/feed）；
//! - [`hub`]：连接注册表与频道广播；**单机直投，多实例经 Redis Pub/Sub 转发**
//!   （v1，[`forward`]；`[realtime].forward = "queue"` + cache 后端 redis）；
//!   经 queue 消费组转发因"同组分摊"语义不适合广播，列为演进项；
//! - [`message`]：实时消息结构（event / topic / payload），与 `queue::Message`
//!   保持一致的序列化约定。

pub mod hub; // 连接注册表与频道广播模块（所有 feature 下均可用）
pub mod message; // 实时消息结构模块（event/topic/payload）

#[cfg(feature = "cache-redis")] // 仅开启 cache-redis 时编译下面的转发模块
pub mod forward; // 跨实例转发（Redis Pub/Sub）模块

#[cfg(feature = "ws")] // 仅开启 ws feature 时编译 WebSocket 模块
pub mod websocket; // WebSocket 升级与单连接生命周期模块
#[cfg(feature = "sse")] // 仅开启 sse feature 时编译 SSE 模块
pub mod sse; // Server-Sent Events 单向推送模块

pub use hub::Hub; // 导出 Hub，供框架与应用的 HasRealtime 使用
pub use message::RealtimeMessage; // 导出 RealtimeMessage，供各推送路径复用
