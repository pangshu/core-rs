//! 测试装配器（feature `testing`）。
//!
//! [`TestApp`] 用**测试默认配置**装配与生产一致的调用链（`AppState` + 完整中间件栈 +
//! `/health`、`/ready`），一行即可发起请求并断言：
//!
//! ```ignore
//! #[tokio::test]
//! async fn health_ok() {
//!     let app = TestApp::new().await;
//!     app.get("/health").await.assert_status_ok();
//! }
//! ```
//!
//! **不做**替代业务测试（见 `docs/03` §4.23）；也可用 `tower::ServiceExt::oneshot`
//! 直打 `Router`（无端口、更快，见 `docs/14` §3）。

pub mod app;

pub use app::{TestApp, test_config};
/// WebSocket 帧类型（`tokio_tungstenite::tungstenite::Message`）。
///
/// 供 WS 端到端测试匹配帧（`WsMessage::Close(...)` 等）。
pub use axum_test::WsMessage;
pub use axum_test::{TestResponse, TestServer, TestWebSocket};
