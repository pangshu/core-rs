//! 压缩层策略。
//!
//! 全局压缩有两个必须回避的副作用：
//!
//! 1. **BREACH**：响应里同时存在「机密」与「攻击者可控的输入」时，压缩后的长度
//!    变化可被逐字节还原。这里跳过显式声明 `Cache-Control: no-store` 的响应
//!    （含机密的响应本就不该被缓存，也应顺带退出压缩）。
//!
//!    注意：`security_headers` 注入的默认 `no-store` 在压缩层**外侧**，
//!    这里看不到；真正敏感的服务应把 `[http].compression.enabled` 关掉，
//!    或在 handler 里显式设置 `Cache-Control: no-store`。
//! 2. **流式响应**：`text/event-stream` / `application/x-ndjson` 靠逐条 flush 传递
//!    增量，压缩会引入缓冲、打乱时序（SSE 表现为「消息延迟成批到达」）。
//!
//! 除此之外仍复用 `tower-http` 的默认判定（体积阈值、内容类型是否可压等）。

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{Extensions, HeaderMap, StatusCode, Version};
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, Predicate};

/// 构造压缩 Layer（默认判定 + 本模块的排除项）。
pub(crate) fn layer() -> CompressionLayer<impl Predicate> {
    let predicate = DefaultPredicate::new()
        .and(should_compress as fn(StatusCode, Version, &HeaderMap, &Extensions) -> bool);
    CompressionLayer::new().compress_when(predicate)
}

/// 本模块的额外排除项（返回 `false` 表示**不压缩**）。
fn should_compress(
    _status: StatusCode,
    _version: Version,
    headers: &HeaderMap,
    _extensions: &Extensions,
) -> bool {
    if header_contains(headers, CACHE_CONTROL, "no-store") {
        return false;
    }
    if header_contains(headers, CONTENT_TYPE, "text/event-stream")
        || header_contains(headers, CONTENT_TYPE, "application/x-ndjson")
    {
        return false;
    }
    true
}

/// 头值是否包含某个子串（大小写不敏感）。
fn header_contains(
    headers: &HeaderMap,
    name: axum::http::header::HeaderName,
    needle: &str,
) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn check(pairs: &[(&str, &str)]) -> bool {
        should_compress(
            StatusCode::OK,
            Version::HTTP_11,
            &headers(pairs),
            &Extensions::new(),
        )
    }

    #[test]
    fn should_skip_no_store_responses() {
        assert!(!check(&[("cache-control", "no-store")]));
        assert!(!check(&[("cache-control", "private, No-Store")]));
        assert!(check(&[("cache-control", "public, max-age=60")]));
        assert!(check(&[]));
    }

    #[test]
    fn should_skip_streaming_content_types() {
        assert!(!check(&[(
            "content-type",
            "text/event-stream; charset=utf-8"
        )]));
        assert!(!check(&[("content-type", "application/x-ndjson")]));
        assert!(check(&[("content-type", "application/json")]));
    }
}
