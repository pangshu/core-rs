//! OpenAPI 文档装配。
//!
//! 业务侧用 `#[derive(OpenApi)]` 声明各 feature 的文档片段，再经
//! [`OpenApiRouter`] 汇总为一份规范，并挂载：
//!
//! - 规范 JSON 端点（`[openapi].json_path`）
//! - 文档 UI（`[openapi].path`）
//!
//! 统一响应体 `ApiResponse<T>` 的 schema 实现见 [`crate::openapi::schema`]。

use axum::Router;
use axum::http::header;
use axum::routing::get;
use utoipa::openapi::{Components, OpenApi};

use crate::app::state::AppState;
use crate::config::OpenApiSection;
use crate::openapi::ui;

/// OpenAPI 文档装配器。
///
/// ```ignore
/// let docs = OpenApiRouter::new().merge::<ApiDoc>();
/// let routes = Router::new().merge(docs.into_routes(&config.openapi));
/// ```
#[derive(Clone, Default)]
pub struct OpenApiRouter {
    spec: OpenApi,
}

impl std::fmt::Debug for OpenApiRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenApiRouter")
            .field("title", &self.spec.info.title)
            .field("paths", &self.spec.paths.paths.len())
            .finish()
    }
}

impl OpenApiRouter {
    /// 新建空文档。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 并入一份 `#[derive(OpenApi)]` 生成的文档。
    #[must_use]
    pub fn merge<T: utoipa::OpenApi>(self) -> Self {
        self.merge_spec(T::openapi())
    }

    /// 并入一份 [`OpenApi`] 规范。
    ///
    /// 合并粒度：`paths`、`components.schemas` / `responses` / `security_schemes`、`tags`；
    /// 同名条目前者保留（后并入者不覆盖），`info` 以首个非空标题者为准。
    #[must_use]
    pub fn merge_spec(mut self, spec: OpenApi) -> Self {
        for (path, item) in spec.paths.paths {
            self.spec.paths.paths.entry(path).or_insert(item);
        }

        if let Some(incoming) = spec.components {
            let target = self.spec.components.get_or_insert_with(Components::new);
            for (name, schema) in incoming.schemas {
                target.schemas.entry(name).or_insert(schema);
            }
            for (name, response) in incoming.responses {
                target.responses.entry(name).or_insert(response);
            }
            for (name, scheme) in incoming.security_schemes {
                target.security_schemes.entry(name).or_insert(scheme);
            }
        }

        if let Some(tags) = spec.tags {
            let target = self.spec.tags.get_or_insert_with(Vec::new);
            for tag in tags {
                if !target.iter().any(|existing| existing.name == tag.name) {
                    target.push(tag);
                }
            }
        }

        if self.spec.info.title.is_empty() && !spec.info.title.is_empty() {
            self.spec.info = spec.info;
        }

        self
    }

    /// 返回当前规范（可用于断言或落盘）。
    #[must_use]
    pub fn spec(&self) -> &OpenApi {
        &self.spec
    }

    /// 序列化为 JSON 字符串。
    ///
    /// # Errors
    /// 规范无法序列化时返回 [`serde_json::Error`]。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        self.spec.to_pretty_json()
    }

    /// 生成可 `merge` 进业务路由的 Router 片段。
    ///
    /// `[openapi].enabled = false` 时返回空 Router（不挂载任何文档路由）。
    pub fn into_routes(self, config: &OpenApiSection) -> Router<AppState> {
        if !config.enabled {
            return Router::new();
        }

        let title = if self.spec.info.title.is_empty() {
            "aurora".to_string()
        } else {
            self.spec.info.title.clone()
        };

        // 规范在装配期一次性序列化：文档是静态的，避免每请求重复序列化。
        let spec_json = match self.spec.to_pretty_json() {
            Ok(json) => json,
            Err(error) => {
                tracing::error!(error = %error, "OpenAPI 规范序列化失败，文档端点将返回空对象");
                "{}".to_string()
            }
        };

        let json_path = config.json_path.clone();
        let ui_path = config.path.clone();

        let json_route = get({
            let spec_json = spec_json.clone();
            move || {
                let body = spec_json.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                        body,
                    )
                }
            }
        });

        let ui_route = get({
            let title = title.clone();
            let json_path = json_path.clone();
            // 资源基址 / SRI 来自 `[openapi]`，装配期取一次
            let assets = ui::UiAssets::from_section(config);
            move || {
                let page = ui::docs_page_with(&title, &json_path, &assets);
                async move { page }
            }
        });

        Router::new()
            .route(&json_path, json_route)
            .route(&ui_path, ui_route)
    }
}

#[cfg(test)]
mod tests {
    use utoipa::openapi::{Info, OpenApiBuilder, PathItem};

    use super::*;

    fn spec_with(path: &str, title: &str) -> OpenApi {
        let mut spec = OpenApiBuilder::new()
            .info(Info::new(title, "1.0.0"))
            .build();
        spec.paths
            .paths
            .insert(path.to_string(), PathItem::default());
        spec
    }

    #[test]
    fn should_merge_paths_from_multiple_specs() {
        let router = OpenApiRouter::new()
            .merge_spec(spec_with("/users", "a"))
            .merge_spec(spec_with("/orders", "b"));

        assert!(router.spec().paths.paths.contains_key("/users"));
        assert!(router.spec().paths.paths.contains_key("/orders"));
        // info 取首个非空标题者
        assert_eq!(router.spec().info.title, "a");
    }

    #[test]
    fn should_keep_first_entry_when_path_collides() {
        let router = OpenApiRouter::new()
            .merge_spec(spec_with("/users", "a"))
            .merge_spec(spec_with("/users", "b"));

        assert_eq!(router.spec().paths.paths.len(), 1);
    }

    #[test]
    fn should_serialize_to_json() {
        let router = OpenApiRouter::new().merge_spec(spec_with("/users", "a"));
        let json = router.to_json().expect("serialize");
        assert!(json.contains("/users"));
    }
}
