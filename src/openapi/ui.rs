//! 文档 UI 页面。
//!
//! 不用 `utoipa-swagger-ui` 的原因（见 `docs/07` §3.18）：该 crate 的
//! **全部**版本都依赖已被 crates.io 全量 yank 的 `zip`（`0.6` / `2.x` / `3.0.0` /
//! `8.6.0`），当前无法解析。这里改为返回一个静态页面：从
//! `[openapi].ui_assets_base` 加载 `swagger-ui-dist` 并指向本服务自己的规范 JSON
//! 端点——无需额外 crate，页面同时给出 JSON 直链，便于离线或内网环境下至少能取到规范。
//! 待上游发布依赖 `zip >= 9` 的版本后切回 `utoipa-swagger-ui`。
//!
//! # 安全边界
//!
//! 页面加载的 JS 与 API **同源执行**（可读 Cookie、可发同源请求）。默认基址是
//! 固定版本的 unpkg CDN，因此生产环境应改为自建静态资源，并配合
//! `[openapi].ui_integrity`（SRI）锁定文件内容。

use axum::response::Html;

/// 文档 UI 的静态资源位置。
#[derive(Debug, Clone)]
pub struct UiAssets {
    /// 资源基址（`swagger-ui.css` / `swagger-ui-bundle.js` 所在目录）。
    pub base: String,
    /// 可选的 SRI（`integrity` 属性）。
    pub integrity: Option<String>,
}

impl UiAssets {
    /// 由配置构造。
    #[must_use]
    pub fn from_section(section: &crate::config::OpenApiSection) -> Self {
        Self {
            base: section.ui_assets_base.trim().to_string(),
            integrity: section
                .ui_integrity
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        }
    }
}

/// 生成文档 UI 页面（使用默认资源基址）。
///
/// `json_path` 指向本服务暴露的 OpenAPI 规范 JSON。
///
/// 返回值 `Html<String>` 本身已是 `#[must_use]`，无需再标注。
pub fn docs_page(title: &str, json_path: &str) -> Html<String> {
    docs_page_with(
        title,
        json_path,
        &UiAssets {
            base: "https://unpkg.com/swagger-ui-dist@5.17.14".to_string(),
            integrity: None,
        },
    )
}

/// 生成文档 UI 页面（可指定资源基址与 SRI）。
///
/// 返回值 `Html<String>` 本身已是 `#[must_use]`，无需再标注。
pub fn docs_page_with(title: &str, json_path: &str, assets: &UiAssets) -> Html<String> {
    let title = escape(title);
    let json_path = escape(json_path);
    let base = escape(assets.base.trim_end_matches('/'));
    // SRI 只会出现在 `integrity="..."` 里，转义掉引号即可
    let integrity = assets
        .integrity
        .as_deref()
        .map(|value| format!(r#" integrity="{}""#, escape(value)))
        .unwrap_or_default();

    let html = format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} · API docs</title>
<link rel="stylesheet" href="{base}/swagger-ui.css" />
<style>body{{margin:0}}</style>
</head>
<body>
<div id="swagger-ui"></div>
<noscript>
  <p style="font-family:sans-serif;padding:1rem">
    需要启用 JavaScript 才能渲染文档。规范文件：<a href="{json_path}">{json_path}</a>
  </p>
</noscript>
<script src="{base}/swagger-ui-bundle.js"{integrity} crossorigin="anonymous"></script>
<script>
  window.addEventListener("load", function () {{
    var el = document.getElementById("swagger-ui");
    if (!window.SwaggerUIBundle) {{
      el.innerHTML =
        '<p style="font-family:sans-serif;padding:1rem">' +
        '无法加载 Swagger UI（离线或被 CSP 拦截）。规范文件：' +
        '<a href="{json_path}">{json_path}</a></p>';
      return;
    }}
    window.ui = window.SwaggerUIBundle({{ url: "{json_path}", dom_id: "#swagger-ui" }});
  }});
</script>
</body>
</html>"##
    );

    Html(html)
}

/// 转义写入 HTML 的文本，避免配置值（标题 / 路径）注入标签或属性。
fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_point_page_at_configured_json_path() {
        let Html(html) = docs_page("myapp", "/api-docs/openapi.json");
        assert!(html.contains("url: \"/api-docs/openapi.json\""));
        assert!(html.contains("<title>myapp · API docs</title>"));
    }

    #[test]
    fn should_pin_asset_base_and_emit_integrity() {
        let assets = UiAssets {
            base: "/docs/assets/".to_string(),
            integrity: Some("sha384-abc123".to_string()),
        };
        let Html(html) = docs_page_with("myapp", "/api-docs/openapi.json", &assets);
        // 去掉尾部斜杠后再拼文件名
        assert!(
            html.contains(r#"href="/docs/assets/swagger-ui.css""#),
            "{html}"
        );
        assert!(
            html.contains(r#"src="/docs/assets/swagger-ui-bundle.js" integrity="sha384-abc123""#),
            "必须带 SRI：{html}"
        );
        assert!(
            !html.contains("unpkg.com"),
            "自建资源不应再引用 CDN：{html}"
        );
    }

    #[test]
    fn should_escape_configured_values() {
        let Html(html) = docs_page("<script>", "\"><img>");
        assert!(!html.contains("<title><script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&quot;&gt;&lt;img&gt;"));
    }
}
