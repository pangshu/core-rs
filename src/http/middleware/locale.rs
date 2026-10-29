//! 语言协商中间件（feature `i18n`）。
//!
//! 按 `Accept-Language` **协商一次**，把结果放进请求扩展；[`Locale`] 提取器
//! 优先读扩展（见 `i18n::Locale::from_request_parts`），因此同一请求内多次
//! 取用不会重复解析。响应同时带 `Content-Language`。
//!
//! 由 `[http].locale.enabled` 控制（默认启用）。中间件关闭时提取器仍会自行
//! 协商，行为一致，只是每个 handler 各算一次。

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::{ACCEPT_LANGUAGE, CONTENT_LANGUAGE};
use axum::middleware::Next;
use axum::response::Response;

use crate::i18n::I18n;

/// 协商语言并注入请求扩展（`enabled = false` 时直接透传）。
pub(crate) async fn handle(i18n: I18n, enabled: bool, mut req: Request, next: Next) -> Response {
    if !enabled {
        return next.run(req).await;
    }

    let locale = i18n.negotiate(
        req.headers()
            .get(ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok()),
    );
    req.extensions_mut().insert(locale.clone());

    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(locale.as_str()) {
        response
            .headers_mut()
            .entry(CONTENT_LANGUAGE)
            .or_insert(value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::I18nSection;
    use crate::i18n::Locale;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    /// 测试用 `I18n`：资源目录不存在 → 只有默认语言受支持。
    fn i18n() -> I18n {
        let section = I18nSection {
            resource_dir: "tests/__no_such_dir__".to_string(),
            default_locale: "zh-CN".to_string(),
            fallback_locale: "en".to_string(),
            ..Default::default()
        };
        I18n::from_config(&section).unwrap()
    }

    fn app(enabled: bool) -> Router {
        let i18n = i18n();
        Router::new()
            .route(
                "/who",
                get(|Extension(locale): Extension<Locale>| async move { locale.as_str().to_string() }),
            )
            .layer(axum::middleware::from_fn(move |req: Request, next: Next| {
                let i18n = i18n.clone();
                async move { handle(i18n, enabled, req, next).await }
            }))
    }

    async fn send(app: Router, accept_language: Option<&str>) -> Response {
        let mut builder = HttpRequest::builder().uri("/who");
        if let Some(value) = accept_language {
            builder = builder.header(ACCEPT_LANGUAGE, value);
        }
        app.oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn should_inject_negotiated_locale() {
        let response = send(app(true), Some("en-US,en;q=0.9")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LANGUAGE)
                .and_then(|value| value.to_str().ok()),
            Some("en")
        );
    }

    #[tokio::test]
    async fn should_fall_back_to_default_locale() {
        let response = send(app(true), None).await;
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LANGUAGE)
                .and_then(|value| value.to_str().ok()),
            Some("zh-CN")
        );
    }

    #[tokio::test]
    async fn should_not_inject_when_disabled() {
        let response = send(app(false), Some("en")).await;
        assert!(response.headers().get(CONTENT_LANGUAGE).is_none());
    }
}
