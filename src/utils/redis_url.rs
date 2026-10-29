//! Redis 连接串的传输安全检查。
//!
//! `docs/11-security-baseline.md` 把 Redis 列为**不可信网络**（须 TLS + 凭据），
//! 但 `redis` crate 的 TLS feature 需要显式启用。这里在配置校验阶段就把
//! 「明文连接串」拦下来，避免运维为了连通性静默降级。

/// 校验 Redis URL 的传输安全性。
///
/// - `rediss://`：通过
/// - `unix://`：通过（本机 socket，不出网络）
/// - `redis://`：仅当 `allow_insecure` 为 `true` 时通过
///
/// # Errors
/// scheme 不受支持，或明文连接未显式放行时返回错误描述。
// 仅被 `[cache]` / `[queue]` 的配置校验调用；未启用这两个 feature 时视为未使用
#[cfg_attr(
    not(any(feature = "cache", feature = "queue")),
    allow(dead_code, reason = "只有 cache / queue 的配置校验会调用")
)]
pub(crate) fn validate(url: &str, allow_insecure: bool) -> Result<(), String> {
    let url = url.trim();
    let scheme = url.split_once("://").map_or("", |(scheme, _)| scheme);
    match scheme {
        "rediss" | "unix" => Ok(()),
        "redis" => {
            if allow_insecure {
                Ok(())
            } else {
                Err(
                    "Redis 连接串使用明文 `redis://`：生产环境必须使用 `rediss://`；\
                     若 Redis 位于可信内网，请显式设置 [cache].redis.allow_insecure = true"
                        .to_string(),
                )
            }
        }
        "" => Err("Redis 连接串缺少 scheme（期望 `rediss://host:port`）".to_string()),
        other => Err(format!(
            "不支持的 Redis URL scheme `{other}://`（期望 `rediss://` 或 `redis://`）"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_tls_and_unix() {
        assert!(validate("rediss://h:6379", false).is_ok());
        assert!(validate("unix:///tmp/redis.sock", false).is_ok());
    }

    #[test]
    fn should_reject_plaintext_by_default() {
        assert!(validate("redis://h:6379", false).is_err());
        assert!(validate("redis://:pass@h:6379/0", false).is_err());
    }

    #[test]
    fn should_accept_plaintext_when_allowed() {
        assert!(validate("redis://h:6379", true).is_ok());
    }

    #[test]
    fn should_reject_unknown_scheme() {
        assert!(validate("http://h:6379", true).is_err());
        assert!(validate("h:6379", true).is_err());
    }
}
