//! `Locale` 提取器：优先取中间件协商好的结果，其次读 `Accept-Language`。

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::header::ACCEPT_LANGUAGE;
use axum::http::request::Parts;

use crate::app::state::AppState;
use crate::i18n::locale::Locale;

impl FromRequestParts<AppState> for Locale {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 优先取 `middleware/locale` 协商好的结果（同一请求只协商一次）
        if let Some(locale) = parts.extensions.get::<Locale>() {
            return Ok(locale.clone());
        }
        let header = parts
            .headers
            .get(ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok());
        Ok(state.i18n.negotiate(header))
    }
}
