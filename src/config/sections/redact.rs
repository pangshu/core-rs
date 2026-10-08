//! 连接串脱敏 [`redact_url`]：各配置节手写 `Debug` 时统一调用，避免凭据进日志。

/// 连接串脱敏（各配置节手写 `Debug` 用）：`scheme://user:pass@host/db` →
/// `scheme://***@host/db`。Settings 及各节大量派生 `#[derive(Debug)]`，
/// 任何一处 `{:?}` 都会把 DB 密码 / JWT secret 打进日志——含敏感字段的节
/// 一律手写 Debug 并经过这里。
pub(crate) fn redact_url(url: &str) -> String { // 对连接串中的凭据做脱敏
    match url.split_once("://") { // 先按协议分隔符切分
        Some((scheme, rest)) => match rest.split_once('@') { // 有协议时再按 @ 切分凭据与主机
            Some((_, host)) => format!("{scheme}://***@{host}"), // 有凭据则用 *** 替换用户名密码
            None => url.to_string(), // 无凭据则原样返回
        },
        None if url.is_empty() => String::new(), // 空串返回空串
        None => "***".to_string(), // 无法解析的非空串整体脱敏为 ***
    }
}
