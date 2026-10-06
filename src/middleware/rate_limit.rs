//! 固定窗口限流（feature = "rate-limit"）：按客户端 IP 经 cache `INCRBY` 计数，
//! 首次计数设定窗口 TTL；超限返回 429 + `Retry-After`。
//!
//! 阈值经 [`HasConfig`] 每请求读取——**支持热更新**（文档 三·4）。
//! 多实例部署需 redis 后端（memory 后端计数进程内有效）。

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::traits::{HasCache, HasConfig};

pub(crate) async fn handle<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasCache + HasConfig + Send + Sync + 'static,
{
    let settings = state.config().load().server.rate_limit.clone();
    if !settings.enabled {
        return next.run(req).await;
    }

    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let mode = crate::utils::client_ip::IpKeyMode::parse(&state.config().load().server.ip_key_mode)
        .unwrap_or(crate::utils::client_ip::IpKeyMode::PeerIp);
    let Some(ip) = (match mode {
        crate::utils::client_ip::IpKeyMode::PeerIp => peer,
        crate::utils::client_ip::IpKeyMode::ProxyHeaders => {
            crate::utils::client_ip::resolve(req.headers(), peer)
        }
    }) else {
        return next.run(req).await;
    };

    let bucket = if settings.bucket.is_empty() {
        "default".to_string()
    } else {
        settings.bucket.clone()
    };
    let window = settings.window_secs.max(1);
    // 固定窗口 key：按当前窗口起点取整，TTL 到窗口结束
    let now = crate::utils::time::now_secs();
    let window_start = now - now % window as i64;
    let key = format!("core-rs:rl:{bucket}:{ip}:{window_start}");

    let cache = state.cache();
    match cache.incr(&key, 1).await {
        Ok(count) => {
            if count == 1 {
                let _ = cache
                    .expire(&key, Some(Duration::from_secs(window + 1)))
                    .await;
            }
            if count > settings.limit as i64 {
                let retry_after = (window as i64 - (now - window_start)).max(1);
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", retry_after.to_string())],
                    axum::Json(crate::web::response::ApiResponse::error(
                        429,
                        "too many requests",
                    )),
                )
                    .into_response();
            }
        }
        // 缓存故障降级放行（与 get_or_load 同一哲学：缓存抖动不传染成业务 500）
        Err(e) => {
            tracing::warn!(error = %e, "rate limit counter unavailable, allowing request");
        }
    }

    next.run(req).await
}
