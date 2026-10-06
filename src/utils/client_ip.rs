//! 客户端真实 IP 解析。
//!
//! - 反代部署：从 `X-Forwarded-For`（取第一个）/ `X-Real-IP` / `Forwarded` 头取；
//! - 直连部署：回退到连接对端地址。
//!
//! 注意：`proxy_headers` 模式信任请求头，只应部署在可信反代之后。

use std::net::IpAddr;

/// 从请求头 + 对端地址解析客户端 IP。`peer` 为 `ConnectInfo<SocketAddr>` 的地址。
pub fn resolve(headers: &axum::http::HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
    for name in ["x-forwarded-for", "x-real-ip", "forwarded"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if let Some(ip) = parse_header(name, v) {
                return Some(ip);
            }
        }
    }
    peer
}

/// 限流 / 幂等等中间件的 key 来源开关（与 `[server.rate_limit].key` 对应）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpKeyMode {
    /// 直连部署：用对端地址
    PeerIp,
    /// 反代部署：从 X-Forwarded-For / X-Real-IP / Forwarded 头取
    ProxyHeaders,
}

impl IpKeyMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "peer_ip" => Some(Self::PeerIp),
            "proxy_headers" => Some(Self::ProxyHeaders),
            _ => None,
        }
    }
}

fn parse_header(name: &str, value: &str) -> Option<IpAddr> {
    match name {
        // X-Forwarded-For: client, proxy1, proxy2 —— 第一个是客户端
        "x-forwarded-for" => value
            .split(',')
            .next()
            .map(|s| s.trim())
            .and_then(|s| s.parse().ok()),
        "x-real-ip" => value.trim().parse().ok(),
        // Forwarded: for=1.2.3.4;host=… —— 取第一个 for
        "forwarded" => value.split(';').find_map(|part| {
            let part = part.trim();
            part.strip_prefix("for=")
                .map(|v| v.trim_matches('"'))
                .and_then(|v| v.parse().ok())
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn xff_first_hop() {
        let h = headers(&[("x-forwarded-for", "1.1.1.1, 10.0.0.1")]);
        assert_eq!(resolve(&h, None), Some("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn real_ip_fallback_to_peer() {
        let h = headers(&[]);
        assert_eq!(resolve(&h, Some("2.2.2.2".parse().unwrap())), Some("2.2.2.2".parse().unwrap()));
        assert_eq!(resolve(&h, None), None);
    }
}
