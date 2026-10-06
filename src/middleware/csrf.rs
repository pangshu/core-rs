//! CSRF 中间件（feature = "csrf"，文档 三·16）：双提交 Cookie 校验——
//! Cookie 里的 `csrf_token` 与请求头 `X-CSRF-Token` 必须一致才放行不安全方法；
//! GET/HEAD/OPTIONS（可配豁免）与无 Cookie 会话的纯 API 流量默认跳过。
//!
//! token 生成与下发：登录页 / 初始化接口调用 [`issue_cookie`] 生成并 Set-Cookie。

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::traits::HasConfig;

/// 生成随机 CSRF token 值（应用自行 Set-Cookie；HttpOnly=false 供前端 JS 读取）
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 构造下发 token 的 Set-Cookie 值（SameSite=Strict + Secure，防跨站带出）。
/// `secure`：HTTPS 部署传 true（HTTP 开发环境传 false，否则浏览器拒收）。
pub fn issue_cookie(cookie_name: &str, token: &str, secure: bool) -> String {
    if secure {
        format!("{cookie_name}={token}; Path=/; SameSite=Strict; Secure")
    } else {
        format!("{cookie_name}={token}; Path=/; SameSite=Strict")
    }
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for cookie_header in headers.get_all(header::COOKIE) {
        let raw = cookie_header.to_str().ok()?;
        for pair in raw.split(';') {
            if let Some((k, v)) = pair.trim().split_once('=') {
                if k == name {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

pub(crate) async fn handle<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasConfig + Send + Sync + 'static,
{
    let csrf = state.config().load().server.csrf.clone();
    if !csrf.enabled {
        return next.run(req).await;
    }
    let exempt = csrf
        .exempt_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(req.method().as_str()));
    if exempt {
        return next.run(req).await;
    }

    let cookie_token = cookie_value(req.headers(), &csrf.cookie_name);
    // 双提交的前提是 token Cookie 已下发：Cookie 完全不存在说明该客户端
    // 不在受 CSRF 保护的会话形态里（纯 API 流量），跳过而非 403 打挂
    let Some(cookie_token) = cookie_token.filter(|c| !c.is_empty()) else {
        return next.run(req).await;
    };
    let header_token = req
        .headers()
        .get(&csrf.header_name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string());

    let ok = matches!(&header_token, Some(h) if *h == cookie_token);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            axum::Json(crate::web::response::ApiResponse::error(
                403,
                "csrf token missing or invalid",
            )),
        )
            .into_response()
    }
}

#[allow(unused)]
fn _keep(h: Option<HeaderValue>) {}
