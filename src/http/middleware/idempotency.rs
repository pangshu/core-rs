//! 幂等性中间件（feature `idempotency`）。
//!
//! 写操作带 `Idempotency-Key` 请求头时，**首次结果会被缓存**；相同键的后续请求
//! 直接回放首次响应（附 `Idempotent-Replay: true`），不再执行业务逻辑。
//!
//! 语义（`docs/09` §6）：
//! - 键相同、指纹相同、首次已完成 → **回放首次结果**
//! - 键相同、首次**仍在处理中** → `30002`（409）
//! - 键相同但请求体指纹不同 → `30002`（409），**不回放**
//! - 未带键 → 不介入（逐请求透传）
//! - **未认证**（无 `Identity`）→ 不介入；若路由声明了 fail-closed → `20001`（401）
//! - 仅缓存 **2xx** 响应；4xx / 5xx 不缓存，便于客户端用同一键重试
//!
//! ## 为什么键必须绑定身份
//!
//! 幂等记录里存的是**完整响应体**。若键只由「方法 + 路径 + 客户端自带的 key」组成，
//! 两个用户用同一个 key 就会互相回放响应；更糟的是本层若位于鉴权之外，未认证请求
//! 也能读到别人的响应。因此：
//!
//! - 本层**必须在 `auth` 内侧**（见 `stack.rs`）；
//! - 键 = `{scope}:{kid}`，`scope` 由 `sub + method + matched_path + query` 派生。
//!
//! **存储**：`state.cache` 的 `Class::Idempotency` 门面（`{ns}:i:{scope}:{kid}`）。
//! 缓存故障时默认 **fail-closed**（返回 `50005`，不执行写操作）：
//! "降级为不校验"会让同一键的写请求重复执行（重复下单 / 重复扣款），
//! 这比"暂时不可用"严重得多。确需降级的场景显式设
//! `[http].idempotency.default_fail_closed = false`（启动期会 warn）。
//!
//! **记录格式**（自定义紧凑二进制，避免为 body 引入 base64）：
//! `[status u16 BE][fp 16][ct_len u8][ct][hdr_count u8]([n_len u8][n][v_len u16 BE][v])*[body]`

use std::time::Duration;

use axum::body::{Body, HttpBody as _, to_bytes};
use axum::extract::{MatchedPath, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::cache::CacheService;
use crate::cache::key::Class;
use crate::config::IdempotencySection;
use crate::errors::{AppError, ErrorCode};

/// 可缓存的响应体上限；超过则不缓存（流式响应没有确定长度，同样跳过）。
const MAX_CACHED_BODY: usize = 1024 * 1024;
/// 「处理中」占位锁的 TTL 下限：防止首个请求崩溃后永久占位。
const IN_FLIGHT_TTL: Duration = Duration::from_secs(30);
/// 幂等键长度上限（避免超长键污染缓存）。
const MAX_KEY_LEN: usize = 128;
/// 幂等键长度下限：1 字符的键可被枚举，等于把「跨用户回放」从理论变成现实。
const MIN_KEY_LEN: usize = 16;
/// 回放标记响应头。
const REPLAY_HEADER: &str = "idempotent-replay";
/// 回放时保留的响应头（只保留与「同一请求的语义」相关的，避免把会话类头带出去）。
const REPLAY_HEADERS: [&str; 4] = ["location", "etag", "content-language", "retry-after"];
/// 请求指纹长度（字节）。
const FINGERPRINT_LEN: usize = 16;

/// 中间件运行期配置（启动时从 `[http].idempotency` 解析一次）。
#[derive(Debug, Clone)]
pub(crate) struct Config {
    enabled: bool,
    ttl: Duration,
    header: HeaderName,
    /// 读取请求体做指纹时的上限（取 `[server].body_limit`）。
    body_limit: usize,
    /// 占位锁 TTL（取 `max(30s, request_timeout + 5s)`）。
    in_flight_ttl: Duration,
    /// fail-closed 路由表。
    fail_closed: Vec<RoutePattern>,
    /// 全局 fail-closed（缓存故障时拒绝写请求）。
    default_fail_closed: bool,
}

/// 一条 fail-closed 路由声明：`"POST /api/v1/orders"` 或 `"POST /api/v1/payments/*"`。
///
/// 路径按**分段**比对，段内可以是字面量或参数占位符：
///
/// - `:id`（axum 0.7 写法）与 `{id}`（axum 0.8 写法）**等价**，都匹配任意一段；
/// - 尾部 `*` 表示前缀匹配（`segments` 视为前缀）。
///
/// 不做纯字符串比较的原因：`MatchedPath` 给的是**路由模板**（`/orders/{id}`），
/// 而配置里可能写成 `:id`、也可能直接写具体路径（`/orders/123`）。字符串比较会让
/// 这些写法「语法合法但永远匹配不上」——资金类路由于是静默退化成 fail-open，
/// 缓存故障时重复扣款且毫无告警。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutePattern {
    method: String,
    segments: Vec<Segment>,
    /// 尾部 `*`：前缀匹配。
    prefix: bool,
}

/// 路由模板中的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// 字面量段。
    Literal(String),
    /// 参数段（`:name` / `{name}` / `{*name}`）：匹配任意一段。
    Param,
}

/// 校验一条 fail-closed 路由声明的格式（供配置校验期调用）。
///
/// # Errors
/// 格式不是 `"<METHOD> <PATH>"`（可带尾部 `*`）、或 `*` 出现在路径中间时返回错误描述。
pub(crate) fn validate_route_pattern(raw: &str) -> Result<(), String> {
    RoutePattern::parse(raw).map(|_| ()).ok_or_else(|| {
        format!(
            "`{raw}` 不是合法的路由声明（期望 `\"POST /api/v1/orders\"`；\
                 `*` 只能出现在路径末尾表示前缀匹配）"
        )
    })
}

impl RoutePattern {
    fn parse(raw: &str) -> Option<Self> {
        let (method, path) = raw.trim().split_once(char::is_whitespace)?;
        let method = method.trim().to_ascii_uppercase();
        let path = path.trim();
        if method.is_empty() || !path.starts_with('/') {
            return None;
        }
        let (path, prefix) = match path.strip_suffix('*') {
            Some(head) => (head.trim_end_matches('/'), true),
            None => (path, false),
        };
        // `*` 只允许出现在尾部：中间的 `*` 会被当成字面量段，永远匹配不上任何真实路径
        if path.contains('*') {
            return None;
        }
        let segments = split_segments(path)
            .map(|segment| {
                if is_param_segment(segment) {
                    Segment::Param
                } else {
                    Segment::Literal(segment.to_string())
                }
            })
            .collect();
        Some(Self {
            method,
            segments,
            prefix,
        })
    }

    fn matches(&self, method: &Method, path: &str) -> bool {
        if self.method != method.as_str() {
            return false;
        }
        let actual: Vec<&str> = split_segments(path).collect();
        if self.prefix {
            if actual.len() < self.segments.len() {
                return false;
            }
        } else if actual.len() != self.segments.len() {
            return false;
        }
        self.segments
            .iter()
            .zip(&actual)
            .all(|(expected, got)| match expected {
                Segment::Literal(literal) => literal == got,
                Segment::Param => !got.is_empty(),
            })
    }
}

/// 按 `/` 切分路径，忽略空段（首尾斜杠、连续斜杠都不影响匹配）。
fn split_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|segment| !segment.is_empty())
}

/// 该段是否是参数占位符（`:id` / `{id}` / `{*id}`）。
fn is_param_segment(segment: &str) -> bool {
    segment.starts_with(':')
        || (segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2)
}

impl Config {
    /// 由配置段构造；请求头名非法时退化为默认值（`validate()` 已在校验期拦截）。
    pub(crate) fn from_section(
        section: &IdempotencySection,
        body_limit: usize,
        request_timeout: Duration,
    ) -> Self {
        let header = HeaderName::from_bytes(section.header.trim().as_bytes())
            .unwrap_or_else(|_| HeaderName::from_static("idempotency-key"));
        let fail_closed = section
            .fail_closed_routes
            .iter()
            .filter_map(|raw| RoutePattern::parse(raw))
            .collect();
        Self {
            enabled: section.enabled,
            ttl: section.ttl,
            header,
            body_limit,
            in_flight_ttl: IN_FLIGHT_TTL.max(request_timeout + Duration::from_secs(5)),
            fail_closed,
            default_fail_closed: section.default_fail_closed,
        }
    }

    /// 本次请求是否按 fail-closed 处理。
    ///
    /// 全局默认（`default_fail_closed`，默认 `true`）与路由级声明是**或**的关系：
    /// 默认已关闭降级，路由表只用于在全局 opt-out 时做例外。
    fn is_fail_closed(&self, method: &Method, paths: &[&str]) -> bool {
        self.default_fail_closed
            || self
                .fail_closed
                .iter()
                .any(|pattern| paths.iter().any(|path| pattern.matches(method, path)))
    }
}

/// 幂等性处理入口。
pub(crate) async fn handle(
    cache: CacheService,
    config: Config,
    req: Request,
    next: Next,
) -> Response {
    if !config.enabled || !is_mutating(req.method()) {
        return next.run(req).await;
    }

    let Some(raw_key) = req
        .headers()
        .get(&config.header)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .map(str::to_string)
    else {
        // 未带幂等键：不介入
        return next.run(req).await;
    };

    if raw_key.len() < MIN_KEY_LEN || raw_key.len() > MAX_KEY_LEN {
        return AppError::biz(
            ErrorCode::INVALID_PARAM,
            format!(
                "{} 长度必须在 {MIN_KEY_LEN}..={MAX_KEY_LEN} 之间",
                config.header
            ),
        )
        .into_response();
    }

    let matched = matched_path(&req);
    // 同时比对**路由模板**与**原始路径**：模板形如 `/orders/{id}`，而配置里
    // 可能写 `:id` 或直接写具体路径；只比一种就会有「配了但永不命中」的盲区。
    let fail_closed = config.is_fail_closed(req.method(), &[matched.as_str(), req.uri().path()]);

    // 未认证：不回放、不缓存（回放等同于绕过鉴权读到别人的响应）
    let Some(subject) = subject(&req) else {
        if fail_closed {
            return AppError::biz(
                ErrorCode::UNAUTHENTICATED,
                format!("{} 需要已认证的主体", config.header),
            )
            .into_response();
        }
        return next.run(req).await;
    };

    let scope = scope_of(&subject, req.method().as_str(), &matched, req.uri().query());
    let kid = short_hex(&raw_key, 16);
    let rest = format!("{scope}:{kid}");

    // 读取请求体做指纹（仅本条路径需要，普通请求不受影响）
    let (parts, body) = req.into_parts();
    let body = match to_bytes(body, config.body_limit).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "幂等请求体读取失败");
            return AppError::biz(ErrorCode::PAYLOAD_TOO_LARGE, "请求体读取失败或超出上限")
                .into_response();
        }
    };
    let fingerprint = short_bytes(&body, FINGERPRINT_LEN);
    let req = Request::from_parts(parts, Body::from(body));

    let store = cache.secure(Class::Idempotency);
    match store.get(&rest).await {
        Ok(Some(record)) => match replay(&record, &fingerprint) {
            Ok(response) => return response,
            Err(replay_error) => return replay_error.into_response(),
        },
        Ok(None) => {}
        Err(error) => {
            if fail_closed {
                tracing::error!(%error, "幂等键读取失败，fail-closed 路由拒绝请求");
                return AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "幂等存储不可用")
                    .into_response();
            }
            tracing::warn!(%error, "幂等键读取失败，本次降级为不校验");
            return next.run(req).await;
        }
    }

    // 抢占「处理中」锁：并发同键请求只有一个能执行，其余直接 409
    let lock = match cache
        .lock(&format!("i:{rest}"), Some(config.in_flight_ttl))
        .await
    {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return AppError::conflict(format!(
                "相同 {} 的请求正在处理中，请稍后重试",
                config.header
            ))
            .into_response();
        }
        Err(error) => {
            if fail_closed {
                tracing::error!(%error, "幂等锁获取失败，fail-closed 路由拒绝请求");
                return AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "幂等锁不可用")
                    .into_response();
            }
            tracing::warn!(%error, "幂等锁获取失败，本次降级为不校验");
            return next.run(req).await;
        }
    };

    // 双检：等锁期间首个请求可能已完成
    if let Ok(Some(record)) = store.get(&rest).await
        && let Ok(response) = replay(&record, &fingerprint)
    {
        return response;
    }

    let response = next.run(req).await;
    if !response.status().is_success() {
        // 失败结果不缓存：允许客户端用同一个键重试
        drop(lock);
        return response;
    }

    let (record, response) = snapshot(response, &fingerprint).await;
    if let Some(record) = record
        && let Err(error) = store.put(&rest, &record, config.ttl).await
    {
        if fail_closed {
            tracing::error!(%error, "幂等结果写入失败（fail-closed 路由，响应已返回）");
        } else {
            tracing::warn!(%error, "幂等结果写入失败（忽略）");
        }
    }
    drop(lock);
    response
}

/// 仅对**写操作**生效。
fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// 路由模板（而非原始路径）：避免 `/orders/123` 这类高基数路径膨胀键空间。
fn matched_path(req: &Request) -> String {
    req.extensions().get::<MatchedPath>().map_or_else(
        || req.uri().path().to_string(),
        |path| path.as_str().to_string(),
    )
}

/// 已认证主体（`auth` feature 关闭时恒为 `None`）。
#[cfg(feature = "auth")]
fn subject(req: &Request) -> Option<String> {
    req.extensions()
        .get::<crate::auth::Identity>()
        .map(|identity| identity.principal.sub.clone())
}

#[cfg(not(feature = "auth"))]
fn subject(_req: &Request) -> Option<String> {
    None
}

/// 作用域：`sub ‖ method ‖ matched_path ‖ query` 的哈希前缀。
fn scope_of(subject: &str, method: &str, path: &str, query: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    for part in [subject, method, path, query.unwrap_or_default()] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hex(&hasher.finalize()[..8])
}

/// 键标识：客户端 key 的哈希前缀（定长，消除分隔符歧义与超长键）。
fn short_hex(raw: &str, bytes: usize) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    hex(&digest[..bytes])
}

fn short_bytes(raw: &[u8], bytes: usize) -> Vec<u8> {
    let digest = Sha256::digest(raw);
    digest[..bytes].to_vec()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // 写入 `String` 不会失败
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 抓取响应快照；不可缓存时原样返回响应。
async fn snapshot(response: Response, fingerprint: &[u8]) -> (Option<Vec<u8>>, Response) {
    let (parts, body) = response.into_parts();
    // `exact()` 为 `None` 表示流式 / 未知长度 → 不缓存（避免把长连接写进缓存）
    let cacheable = body
        .size_hint()
        .exact()
        .is_some_and(|len| len <= MAX_CACHED_BODY as u64);
    if !cacheable {
        tracing::warn!(
            status = %parts.status,
            "幂等响应为流式或超过 1 MiB，未缓存（客户端重试会重新执行）"
        );
        return (None, Response::from_parts(parts, body));
    }

    match to_bytes(body, MAX_CACHED_BODY).await {
        Ok(bytes) => {
            let record = encode(&parts, fingerprint, &bytes);
            (Some(record), Response::from_parts(parts, Body::from(bytes)))
        }
        Err(error) => {
            // 已按 `size_hint` 预判上限，理论上不可达；保守返回 500 而非丢失响应体
            tracing::error!(%error, "幂等响应体读取失败");
            (
                None,
                AppError::internal(anyhow::anyhow!("idempotency body read failed")).into_response(),
            )
        }
    }
}

/// 编码记录：`[status][fp][ct][headers][body]`。
fn encode(parts: &axum::http::response::Parts, fingerprint: &[u8], body: &[u8]) -> Vec<u8> {
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut ct = content_type.as_bytes();
    if ct.len() > u8::MAX as usize {
        ct = &[];
    }

    let headers: Vec<(&[u8], &[u8])> = REPLAY_HEADERS
        .iter()
        .filter_map(|name| {
            let value = parts.headers.get(*name)?.to_str().ok()?;
            Some((name.as_bytes(), value.as_bytes()))
        })
        .filter(|(_, value)| value.len() <= u16::MAX as usize)
        .collect();

    let mut out = Vec::with_capacity(32 + ct.len() + body.len());
    out.extend_from_slice(&parts.status.as_u16().to_be_bytes());
    out.extend_from_slice(fingerprint);
    out.push(ct.len() as u8);
    out.extend_from_slice(ct);
    out.push(headers.len() as u8);
    for (name, value) in headers {
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
    }
    out.extend_from_slice(body);
    out
}

/// 回放首次结果；指纹不一致返回 409（**不回放**）。
fn replay(record: &[u8], fingerprint: &[u8]) -> Result<Response, AppError> {
    let Some(decoded) = decode(record) else {
        tracing::error!(len = record.len(), "幂等记录损坏");
        return Err(AppError::internal(anyhow::anyhow!(
            "idempotent record corrupted"
        )));
    };

    if decoded.fingerprint != fingerprint {
        return Err(AppError::conflict(
            "相同幂等键的请求体与首次请求不一致，已拒绝回放",
        ));
    }

    let mut builder = Response::builder()
        .status(decoded.status)
        .header(REPLAY_HEADER, "true");
    if !decoded.content_type.is_empty() {
        builder = builder.header(CONTENT_TYPE, decoded.content_type);
    }
    for (name, value) in decoded.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(decoded.body.to_vec()))
        .map_err(|error| {
            AppError::internal(anyhow::anyhow!("rebuild idempotent response: {error}"))
        })
}

struct Decoded<'a> {
    status: StatusCode,
    fingerprint: &'a [u8],
    content_type: &'a [u8],
    headers: Vec<(HeaderName, HeaderValue)>,
    body: &'a [u8],
}

fn decode(record: &[u8]) -> Option<Decoded<'_>> {
    let status =
        StatusCode::from_u16(u16::from_be_bytes([*record.first()?, *record.get(1)?])).ok()?;
    let fingerprint = record.get(2..2 + FINGERPRINT_LEN)?;
    let ct_len = *record.get(2 + FINGERPRINT_LEN)? as usize;
    let ct_start = 3 + FINGERPRINT_LEN;
    let content_type = record.get(ct_start..ct_start + ct_len)?;
    let count_at = ct_start + ct_len;
    let header_count = *record.get(count_at)? as usize;
    let mut cursor = count_at + 1;
    let mut headers = Vec::with_capacity(header_count);
    for _ in 0..header_count {
        let name_len = *record.get(cursor)? as usize;
        cursor += 1;
        let name = HeaderName::from_bytes(record.get(cursor..cursor + name_len)?).ok()?;
        cursor += name_len;
        let value_len =
            u16::from_be_bytes([*record.get(cursor)?, *record.get(cursor + 1)?]) as usize;
        cursor += 2;
        let value = HeaderValue::from_bytes(record.get(cursor..cursor + value_len)?).ok()?;
        cursor += value_len;
        headers.push((name, value));
    }
    let body = record.get(cursor..)?;
    Some(Decoded {
        status,
        fingerprint,
        content_type,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::ServiceExt;

    use super::*;
    use crate::config::CacheSection;

    async fn test_cache() -> CacheService {
        let section = CacheSection {
            namespace: "test".to_string(),
            ..Default::default()
        };
        CacheService::from_config(&section).await.unwrap()
    }

    fn config() -> Config {
        let section = IdempotencySection {
            enabled: true,
            ttl: Duration::from_secs(60),
            ..Default::default()
        };
        Config::from_section(&section, 2 * 1024 * 1024, Duration::from_secs(10))
    }

    async fn app(cache: CacheService) -> (Router, Arc<AtomicU32>) {
        app_with(cache, config()).await
    }

    /// 指定运行期配置装配（用于对比 fail-closed 开 / 关两种行为）。
    async fn app_with(cache: CacheService, config: Config) -> (Router, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        let router = Router::new()
            .route(
                "/write",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                        format!("run-{n}")
                    }
                }),
            )
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let cache = cache.clone();
                    let config = config.clone();
                    async move { handle(cache, config, req, next).await }
                },
            ));
        (router, hits)
    }

    fn request(key: Option<&str>, body: &str, subject: Option<&str>) -> Request {
        let mut builder = Request::builder().method(Method::POST).uri("/write");
        if let Some(key) = key {
            builder = builder.header("idempotency-key", key);
        }
        #[cfg(feature = "auth")]
        {
            let mut request = builder.body(Body::from(body.to_string())).unwrap();
            if let Some(subject) = subject {
                request.extensions_mut().insert(crate::auth::Identity::jwt(
                    crate::auth::Principal::new(subject),
                    0,
                ));
            }
            request
        }
        #[cfg(not(feature = "auth"))]
        {
            let _ = subject;
            builder.body(Body::from(body.to_string())).unwrap()
        }
    }

    #[cfg(feature = "auth")]
    const KEY: &str = "0123456789abcdef";

    #[cfg(feature = "auth")]
    #[tokio::test]
    async fn should_replay_first_response_for_same_key() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        let first = router
            .clone()
            .oneshot(request(Some(KEY), "{}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(REPLAY_HEADER).is_none());

        let second = router
            .oneshot(request(Some(KEY), "{}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(REPLAY_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "第二次不应执行业务逻辑");
    }

    #[cfg(feature = "auth")]
    #[tokio::test]
    async fn should_not_replay_without_identity() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        router
            .clone()
            .oneshot(request(Some(KEY), "{}", Some("u1")))
            .await
            .unwrap();
        // 未认证请求：不得读到别人的缓存响应。
        // 默认 `default_fail_closed = true`，所以这里是**直接 401**，
        // 而不是「放行但跳过回放」——两者都不泄漏响应，前者更严格。
        let anonymous = router
            .oneshot(request(Some(KEY), "{}", None))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert!(
            anonymous.headers().get(REPLAY_HEADER).is_none(),
            "未认证请求不得回放"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "fail-closed 下匿名请求不应执行到业务逻辑"
        );
    }

    /// 关掉 `default_fail_closed` 后，匿名请求改为「放行但不回放」。
    #[cfg(feature = "auth")]
    #[tokio::test]
    async fn should_fall_through_without_identity_when_fail_closed_disabled() {
        let cache = test_cache().await;
        let config = Config {
            default_fail_closed: false,
            ..config()
        };
        let (router, hits) = app_with(cache, config).await;

        router
            .clone()
            .oneshot(request(Some(KEY), "{}", Some("u1")))
            .await
            .unwrap();
        let anonymous = router
            .oneshot(request(Some(KEY), "{}", None))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::OK);
        assert!(
            anonymous.headers().get(REPLAY_HEADER).is_none(),
            "未认证请求不得回放别人的响应"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "非 fail-closed 下匿名请求应重新执行业务逻辑"
        );
    }

    #[cfg(feature = "auth")]
    #[tokio::test]
    async fn should_not_share_between_subjects() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        router
            .clone()
            .oneshot(request(Some(KEY), "{}", Some("alice")))
            .await
            .unwrap();
        let bob = router
            .oneshot(request(Some(KEY), "{}", Some("bob")))
            .await
            .unwrap();
        assert!(
            bob.headers().get(REPLAY_HEADER).is_none(),
            "不同主体不得互放"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[cfg(feature = "auth")]
    #[tokio::test]
    async fn should_conflict_on_different_body() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        router
            .clone()
            .oneshot(request(Some(KEY), "{\"a\":1}", Some("u1")))
            .await
            .unwrap();
        let second = router
            .oneshot(request(Some(KEY), "{\"a\":2}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::CONFLICT,
            "同键不同 body 应 409"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "auth")]
    #[tokio::test]
    async fn should_execute_for_different_keys() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        router
            .clone()
            .oneshot(request(Some("aaaaaaaaaaaaaaaa"), "{}", Some("u1")))
            .await
            .unwrap();
        router
            .oneshot(request(Some("bbbbbbbbbbbbbbbb"), "{}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn should_not_intervene_without_key() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        router
            .clone()
            .oneshot(request(None, "{}", Some("u1")))
            .await
            .unwrap();
        router
            .oneshot(request(None, "{}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn should_reject_too_short_or_too_long_key() {
        let cache = test_cache().await;
        let (router, hits) = app(cache).await;

        let short = router
            .clone()
            .oneshot(request(Some("short"), "{}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(short.status(), StatusCode::BAD_REQUEST, "1 字符键必须拒绝");

        let long = "k".repeat(MAX_KEY_LEN + 1);
        let response = router
            .oneshot(request(Some(&long), "{}", Some("u1")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn should_parse_fail_closed_routes() {
        let pattern = RoutePattern::parse("POST /api/v1/payments/*").unwrap();
        assert!(pattern.matches(&Method::POST, "/api/v1/payments/1"));
        assert!(pattern.matches(&Method::POST, "/api/v1/payments"));
        assert!(!pattern.matches(&Method::GET, "/api/v1/payments/1"));
        assert!(!pattern.matches(&Method::POST, "/api/v1/payment/1"));

        let exact = RoutePattern::parse("post /api/v1/orders").unwrap();
        assert!(exact.matches(&Method::POST, "/api/v1/orders"));
        assert!(exact.matches(&Method::POST, "/api/v1/orders/"));
        assert!(!exact.matches(&Method::POST, "/api/v1/orders/1"));

        assert!(RoutePattern::parse("nonsense").is_none());
        assert!(RoutePattern::parse("POST orders").is_none());
        // `*` 只允许在尾部：中间的 `*` 会变成永远匹配不上的字面量段
        assert!(RoutePattern::parse("POST /api/*/orders").is_none());
    }

    /// 回归：`:id` / `{id}` / 尾部 `*` 三种写法都必须命中同一路由。
    ///
    /// 修复前用字符串前缀比较，`MatchedPath` 给的是 `/orders/{id}`，配置写
    /// `/orders/:id` 就永远匹配不上 → 资金类路由静默 fail-open。
    #[test]
    fn should_match_route_template_regardless_of_param_syntax() {
        for declared in [
            "POST /api/v1/orders/:id",
            "POST /api/v1/orders/{id}",
            "POST /api/v1/orders/*",
        ] {
            let pattern = RoutePattern::parse(declared).unwrap();
            for actual in [
                "/api/v1/orders/{id}",
                "/api/v1/orders/:id",
                "/api/v1/orders/123",
            ] {
                assert!(
                    pattern.matches(&Method::POST, actual),
                    "声明 `{declared}` 应命中 `{actual}`"
                );
            }
        }

        // 参数段只吃一段；尾部 `*` 才是前缀匹配
        let single = RoutePattern::parse("POST /api/v1/orders/:id").unwrap();
        assert!(!single.matches(&Method::POST, "/api/v1/orders/123/items"));
        let prefix = RoutePattern::parse("POST /api/v1/orders/*").unwrap();
        assert!(prefix.matches(&Method::POST, "/api/v1/orders/123/items"));
    }

    /// 字面量段只匹配字面量：配置写死具体 id 时，模板里的 `{id}` 不算命中。
    #[test]
    fn should_match_literal_segment_exactly() {
        let pattern = RoutePattern::parse("POST /api/v1/orders/123").unwrap();
        assert!(pattern.matches(&Method::POST, "/api/v1/orders/123"));
        assert!(!pattern.matches(&Method::POST, "/api/v1/orders/{id}"));
        assert!(!pattern.matches(&Method::POST, "/api/v1/orders/124"));
    }

    #[test]
    fn should_match_whole_path_when_fail_closed_is_exact() {
        // 关掉全局 fail-closed，本用例只验「路由级声明」的匹配语义；
        // 否则 `default_fail_closed = true` 会让任何方法都命中，断言失去意义。
        let config = Config {
            default_fail_closed: false,
            fail_closed: vec![RoutePattern::parse("POST /api/v1/orders").unwrap()],
            ..config()
        };
        // 模板与原始路径任一命中即可
        assert!(config.is_fail_closed(&Method::POST, &["/api/v1/orders", "/api/v1/orders"]));
        assert!(config.is_fail_closed(&Method::POST, &["/api/v1/orders", "/ignored"]));
        assert!(!config.is_fail_closed(&Method::GET, &["/api/v1/orders", "/api/v1/orders"]));
        assert!(!config.is_fail_closed(&Method::POST, &["/api/v1/users", "/api/v1/users"]));
    }

    #[test]
    fn should_encode_and_decode_record() {
        let mut response = Response::builder()
            .status(StatusCode::CREATED)
            .header(CONTENT_TYPE, "application/json")
            .header("location", "/orders/1")
            .body(Body::from("{}"))
            .unwrap();
        let parts = response.headers_mut();
        assert_eq!(parts.len(), 2);

        let (parts, _) = response.into_parts();
        let fingerprint = [7u8; FINGERPRINT_LEN];
        let record = encode(&parts, &fingerprint, b"{\"id\":1}");
        let decoded = decode(&record).unwrap();
        assert_eq!(decoded.status, StatusCode::CREATED);
        assert_eq!(decoded.content_type, b"application/json");
        assert_eq!(decoded.fingerprint, &fingerprint);
        assert_eq!(decoded.body, b"{\"id\":1}");
        assert_eq!(decoded.headers.len(), 1, "只保留白名单响应头");
        assert_eq!(decoded.headers[0].0.as_str(), "location");
    }
}
