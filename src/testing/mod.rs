//! 测试装配器（feature = "testing"，文档 三·17）：`TestApp::new()` 一行拿到
//! 可发请求的完整 App——自动 `APP_ENV=testing` 语义（程序化配置）→ 连测试库
//! （sqlite 内存库）→ 跑迁移 → 返回带请求助手的实例。
//!
//! 应用的 `tests/` 集成测试直接复用，不必每个项目重造「起服务、造数据、
//! 断言响应」的脚手架：
//!
//! ```rust,ignore
//! let app = TestApp::builder().build(|core| AppState { core }).await?;
//! app.mount(user_routes());
//! let res = app.get("/users?page=1&size=10").await;
//! assert_eq!(res.status(), 200);
//! ```

pub mod app; // 声明测试装配器实现子模块（TestApp / TestAppBuilder）

pub use app::{TestApp, TestAppBuilder}; // 重新导出测试应用与构建器，供集成测试直接使用
