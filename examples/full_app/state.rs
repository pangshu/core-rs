//! full_app 的示例私有状态。
//!
//! `AppState` 是框架状态（config / db / cache / queue / auth / ...）；业务上下文
//! 不进 `AppState`，而是通过 [`axum::Extension`] 注入 handler。

use aurora::realtime::Hub;

/// 业务上下文。
#[derive(Clone)]
pub struct DemoCtx {
    /// 实时广播中心：SSE（`/events`）与 WS（`/ws/{topic}`）共用。
    pub hub: Hub,
}

impl DemoCtx {
    /// 按 `[realtime]` 默认配置创建。
    #[must_use]
    pub fn new() -> Self {
        let hub = Hub::new();
        // 订阅授权是 fail-closed：必须注册，否则 `/events` 与 `/ws/{topic}` 一律拒绝。
        // 示例用单频道 `todos`，生产里应校验频道归属（如 `user:{sub}`）。
        hub.on_subscribe(|req| req.topic == "todos");
        Self { hub }
    }
}

impl Default for DemoCtx {
    fn default() -> Self {
        Self::new()
    }
}
