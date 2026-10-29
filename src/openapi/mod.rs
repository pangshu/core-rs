//! OpenAPI 文档（feature `openapi`）。
//!
//! 让接口**可自描述**：业务侧用 `#[derive(OpenApi)]` 声明各 feature 的文档片段，
//! 经 [`OpenApiRouter`] 汇总后挂载规范 JSON 端点与文档 UI。
//!
//! - [`OpenApiRouter`]：文档装配器（`merge` / `merge_spec` / `into_routes`）
//! - [`docs_page`]：文档 UI 页面（静态页 + CDN 版 Swagger UI）
//! - `schema`：统一响应体 [`ApiResponse<T>`](crate::http::ApiResponse) 的泛型 schema
//!
//! 配置见 `docs/08` §3 `[openapi]`；验收标准见 `docs/06` M4。

mod doc;
mod schema;
mod ui;

pub use doc::OpenApiRouter;
pub use ui::docs_page;
