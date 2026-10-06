//! 写接口幂等（文档 三·10）：读 `Idempotency-Key` 头，首次请求占位（锁）→
//! 执行 → 缓存响应；重复请求直接回放结果，避免写接口重放。
//!
//! 只作用于带 body 的写方法（POST/PUT/PATCH）且带 key 的请求；无 Redis 时
//! 自动降级为进程内实现（单机可用，多实例需 Redis）。

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::cache::CacheExt;
use crate::traits::{HasCache, HasConfig};

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
/// 客户端提供的 key 长度上限：防恶意超长 key 撑爆缓存键空间
const MAX_KEY_LEN: usize = 255;

pub(crate) async fn handle<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasCache + HasConfig + Send + Sync + 'static,
{
    let idem = state.config().load().server.idempotency.clone();
    if !idem.enabled {
        return next.run(req).await;
    }
    let is_write = matches!(
        *req.method(),
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH
    );
    let key = req
        .headers()
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if !is_write || key.is_empty() {
        return next.run(req).await;
    }
    if key.len() > MAX_KEY_LEN {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(crate::web::response::ApiResponse::error(
                400,
                "Idempotency-Key too long (max 255 bytes)",
            )),
        )
            .into_response();
    }

    // 缓存键按 身份+方法+路由 隔离：key 只来自客户端可伪造的请求头，
    // 不隔离则同 key 的不同用户/不同接口会互相回放对方的响应体
    let user = req
        .extensions()
        .get::<crate::auth::Identity>()
        .map(|i| i.id.as_str())
        .unwrap_or("anon");
    let scope = format!("{}:{}:{}", user, req.method(), req.uri().path());
    let cache_key = format!("core-rs:idem:{scope}:{key}");
    let lock_key = format!("core-rs:idem-lock:{scope}:{key}");
    let ttl = Duration::from_secs(idem.ttl_secs.max(1));

    // 已有回放结果：直接回放（锁外快路径）
    if let Ok(Some(replay)) = state.cache().get_json::<Replay>(&cache_key).await {
        return replay.into_response();
    }

    // 占位锁：并发同 key 请求中，后来者读到回放缓存或得到 409
    let guard = state.lock().clone().try_acquire(&lock_key, ttl).await;
    let guard = match guard {
        Ok(Some(g)) => g,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(crate::web::response::ApiResponse::error(
                    409,
                    "request with this Idempotency-Key is already in progress",
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!(error = %e, "idempotency lock unavailable, executing without guard");
            // 锁故障降级直执（不阻塞业务）
            return execute_and_cache(state, req, next, cache_key, idem.max_body_bytes, ttl).await;
        }
    };

    // 拿到锁后二次读（双检）
    if let Ok(Some(replay)) = state.cache().get_json::<Replay>(&cache_key).await {
        let _ = guard.release().await;
        return replay.into_response();
    }
    let res = execute_and_cache(state, req, next, cache_key, idem.max_body_bytes, ttl).await;
    let _ = guard.release().await;
    res
}

async fn execute_and_cache<S>(
    state: S,
    req: Request,
    next: Next,
    cache_key: String,
    max_body: usize,
    ttl: Duration,
) -> Response
where
    S: HasCache + HasConfig + Send + Sync + 'static,
{
    let res = next.run(req).await;

    // 只缓存可回放的 JSON 响应（4xx/5xx 不缓存：失败允许重试）
    let status = res.status();
    if !status.is_success() {
        return res;
    }
    let (parts, body) = res.into_parts();
    // Content-Length 已知超限：不读 body，原样透传（不缓存，也绝不丢业务数据）
    if let Some(len) = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if len > max_body {
            return Response::from_parts(parts, body);
        }
    }
    match axum::body::to_bytes(body, max_body.max(1)).await {
        Ok(bytes) => {
            let replay = Replay {
                status: status.as_u16(),
                body: bytes.to_vec(),
                content_type: parts
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string(),
            };
            let _ = state.cache().set_json(&cache_key, &replay, Some(ttl)).await;
            Response::from_parts(parts, axum::body::Body::from(bytes))
        }
        // 响应体超限（流式/无 Content-Length）：显式报错而非静默丢业务数据
        Err(_) => {
            tracing::warn!(
                limit = max_body,
                "idempotency: response body exceeded max_body_bytes and cannot be replayed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::web::response::ApiResponse::error(
                    500,
                    "response body too large to be made idempotent",
                )),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Replay {
    status: u16,
    body: Vec<u8>,
    content_type: String,
}

impl Replay {
    fn into_response(self) -> Response {
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK),
            [(axum::http::header::CONTENT_TYPE, self.content_type)],
            self.body,
        )
            .into_response()
    }
}
