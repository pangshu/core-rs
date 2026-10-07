//! 客户端真实 IP 解析。
//!
//! - 反代部署：从 `X-Forwarded-For`（取第一个）/ `X-Real-IP` / `Forwarded` 头取；
//! - 直连部署：回退到连接对端地址。
//!
//! 注意：`proxy_headers` 模式信任请求头，只应部署在可信反代之后。

use std::net::IpAddr; // 引入 IP 地址类型

/// 从请求头 + 对端地址解析客户端 IP。`peer` 为 `ConnectInfo<SocketAddr>` 的地址。
pub fn resolve(headers: &axum::http::HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> { // 综合请求头与对端地址解析客户端 IP
    for name in ["x-forwarded-for", "x-real-ip", "forwarded"] { // 按优先级依次尝试三种代理头
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) { // 取头部值并转为可读字符串
            if let Some(ip) = parse_header(name, v) { // 按头部格式解析出 IP
                return Some(ip); // 解析成功即返回该 IP
            }
        }
    }
    peer // 无可用代理头时回退到连接对端地址
}

/// 限流 / 幂等等中间件的 key 来源开关（与 `[server.rate_limit].key` 对应）
#[derive(Debug, Clone, Copy, PartialEq, Eq)] // 派生调试、拷贝、相等比较等能力
pub enum IpKeyMode { // 中间件 key 取 IP 的模式枚举
    /// 直连部署：用对端地址
    PeerIp, // 使用连接对端地址
    /// 反代部署：从 X-Forwarded-For / X-Real-IP / Forwarded 头取
    ProxyHeaders, // 使用代理请求头中的客户端 IP
}

impl IpKeyMode { // 模式解析方法
    pub fn parse(s: &str) -> Option<Self> { // 把配置字符串解析为模式枚举
        match s { // 按字符串分派
            "" | "peer_ip" => Some(Self::PeerIp), // 空串或 peer_ip 对应直连模式
            "proxy_headers" => Some(Self::ProxyHeaders), // proxy_headers 对应反代模式
            _ => None, // 其他取值非法返回 None
        }
    }
}

fn parse_header(name: &str, value: &str) -> Option<IpAddr> { // 按具体头部格式解析出 IP
    match name { // 按头部名分派
        // X-Forwarded-For: client, proxy1, proxy2 —— 第一个是客户端
        "x-forwarded-for" => value // 解析 XFF 取第一个
            .split(',') // 按逗号拆分多跳
            .next() // 取第一个（即最原始客户端）
            .map(|s| s.trim()) // 去除空白
            .and_then(|s| s.parse().ok()), // 解析为 IP
        "x-real-ip" => value.trim().parse().ok(), // X-Real-IP 直接 trim 后解析
        // Forwarded: for=1.2.3.4;host=… —— 取第一个 for
        "forwarded" => value.split(';').find_map(|part| { // 按分号拆分各参数并找 for
            let part = part.trim(); // 去除参数空白
            part.strip_prefix("for=") // 只处理 for= 参数
                .map(|v| v.trim_matches('"')) // 去掉可能包裹的引号
                .and_then(|v| v.parse().ok()) // 解析为 IP
        }),
        _ => None, // 未知头部返回 None
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
