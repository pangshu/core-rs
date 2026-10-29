//! AMQP 连接串的传输安全检查。
//!
//! `docs/11-security-baseline.md` 把消息中间件列为**不可信网络**（须 TLS + 凭据）。
//! RabbitMQ 的 `amqp://` 会把用户名 / 密码与消息体明文送出网卡，而 `lapin` 不会
//! 因为 scheme 是 `amqp` 就给出任何提示。这里在配置校验阶段就拦下明文连接，
//! 避免运维为了连通性静默降级——与 `[cache].redis.allow_insecure` 同一套模式。

/// 校验 AMQP URL 的传输安全性。
///
/// - `amqps://`：通过
/// - `amqp://`：仅当 `allow_insecure` 为 `true` 时通过
///
/// # Errors
/// scheme 不受支持，或明文连接未显式放行时返回错误描述。
// 仅被 `[queue]` 的配置校验调用；未启用 `queue` feature 时视为未使用
#[cfg_attr(
    not(feature = "queue"),
    allow(dead_code, reason = "只有 queue 的配置校验会调用")
)]
pub(crate) fn validate(url: &str, allow_insecure: bool) -> Result<(), String> {
    let url = url.trim();
    let scheme = url.split_once("://").map_or("", |(scheme, _)| scheme);
    match scheme {
        "amqps" => Ok(()),
        "amqp" => {
            if allow_insecure {
                Ok(())
            } else {
                Err(
                    "AMQP 连接串使用明文 `amqp://`：凭据与消息体都会明文过网，生产环境必须使用 \
                     `amqps://`；若 RabbitMQ 位于可信内网，请显式设置 [queue].amqp.allow_insecure = true"
                        .to_string(),
                )
            }
        }
        "" => Err("AMQP 连接串缺少 scheme（期望 `amqps://user:pass@host:5671/%2f`）".to_string()),
        other => Err(format!(
            "不支持的 AMQP URL scheme `{other}://`（期望 `amqps://` 或 `amqp://`）"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_tls() {
        assert!(validate("amqps://h:5671/%2f", false).is_ok());
    }

    #[test]
    fn should_reject_plaintext_by_default() {
        assert!(validate("amqp://h:5672/%2f", false).is_err());
        assert!(validate("amqp://user:pass@h:5672/%2f", false).is_err());
    }

    #[test]
    fn should_accept_plaintext_when_allowed() {
        assert!(validate("amqp://h:5672/%2f", true).is_ok());
    }

    #[test]
    fn should_reject_unknown_scheme() {
        assert!(validate("http://h:5672", true).is_err());
        assert!(validate("h:5672", true).is_err());
    }
}
