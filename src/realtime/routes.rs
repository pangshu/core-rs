//! 实时端点路由：一次性挂载 WS 与 SSE。

use crate::app::AppState;
use crate::axum::Router;
use crate::axum::routing::get;
use crate::realtime::hub::Hub;

/// 统一入口：一次性挂载 `/ws/{topic}` 与 `/events`。
///
/// ⚠️ **必须先注册订阅授权钩子**（[`Hub::on_subscribe`]）——未注册时所有实时端点
/// 一律按 fail-closed 拒绝（`50001`）。这里在装配期打一条 `warn` 提前暴露漏配，
/// 免得等到第一个连接进来才发现。
///
/// ```no_run
/// use aurora::config::AppConfig;
/// use aurora::realtime::{Hub, handle};
///
/// let config = AppConfig::default();
/// let hub = Hub::from_config(&config.realtime);
/// hub.on_subscribe(|req| req.topic.starts_with("public:"));
/// hub.on_message(|msg| tracing::info!(topic = %msg.topic, "收到上行"));
/// let _routes = handle(hub);
/// ```
pub fn handle(hub: Hub) -> Router<AppState> {
    #[cfg_attr(not(any(feature = "ws", feature = "sse")), allow(unused_mut))]
    let mut router = Router::new();

    #[cfg(any(feature = "ws", feature = "sse"))]
    if !hub.has_subscribe_authorizer() {
        tracing::warn!(
            "realtime::handle 未注册订阅授权钩子（Hub::on_subscribe）：所有实时端点将按 \
             fail-closed 拒绝。频道名通常是可猜的业务 ID，请务必校验频道归属"
        );
    }

    #[cfg(feature = "ws")]
    {
        router = router.route(
            "/ws/{topic}",
            get(crate::realtime::websocket::handler(hub.clone())),
        );
    }
    #[cfg(feature = "sse")]
    {
        router = router.route("/events", get(crate::realtime::sse::stream(hub.clone())));
    }

    #[cfg(not(any(feature = "ws", feature = "sse")))]
    let _ = hub;

    router
}
