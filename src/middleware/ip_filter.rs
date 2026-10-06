//! IP 过滤：IPv4/IPv6 CIDR 黑白名单（`allow` 非空时仅放行列表内来源；
//! `deny` 命中一律拒绝）。key 来源与限流一致（`[server].ip_key_mode`）。
//!
//! 解析纪律：**规则解析失败必须 fail-fast**（`from_rules` 返回 `Result`，
//! `CoreState::from_settings` 启动期校验）——静默丢弃 + 空 allow 短路为全放行
//! 组合会变成 fail-open（只配 IPv6 白名单时公网 IP 全部放行）。
//! IPv4-mapped IPv6（`::ffff:a.b.c.d`，双栈下常见 peer 形态）匹配前归一化为 IPv4。

use std::net::IpAddr;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::traits::HasConfig;

/// 解析后的过滤规则
#[derive(Debug, Clone, Default)]
pub struct IpFilter {
    allow: Vec<Cidr>,
    deny: Vec<Cidr>,
}

#[derive(Debug, Clone)]
enum Cidr {
    V4 { addr: std::net::Ipv4Addr, prefix: u8 },
    V6 { addr: std::net::Ipv6Addr, prefix: u8 },
    Exact(IpAddr),
}

impl Cidr {
    fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some((addr, prefix)) = s.split_once('/') {
            let ip: IpAddr = addr
                .parse()
                .map_err(|_| format!("invalid address in CIDR {s:?}"))?;
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| format!("invalid prefix in CIDR {s:?}"))?;
            match ip {
                IpAddr::V4(a) if prefix <= 32 => Ok(Self::V4 { addr: a, prefix }),
                IpAddr::V6(a) if prefix <= 128 => Ok(Self::V6 { addr: a, prefix }),
                IpAddr::V4(_) => Err(format!("IPv4 prefix out of range in {s:?} (0-32)")),
                IpAddr::V6(_) => Err(format!("IPv6 prefix out of range in {s:?} (0-128)")),
            }
        } else {
            s.parse::<IpAddr>()
                .map(Self::Exact)
                .map_err(|_| format!("invalid IP address {s:?}"))
        }
    }

    fn matches(&self, ip: &IpAddr) -> bool {
        match self {
            Self::Exact(x) => x == ip,
            Self::V4 { addr, prefix } => match ip {
                IpAddr::V4(other) => {
                    let a = u32::from(*addr);
                    let b = u32::from(*other);
                    let mask = v4_mask(*prefix);
                    a & mask == b & mask
                }
                // IPv4 规则不匹配纯 IPv6 地址（mapped 形态已在入口归一化）
                IpAddr::V6(_) => false,
            },
            Self::V6 { addr, prefix } => match ip {
                IpAddr::V6(other) => {
                    let a = u128::from(*addr);
                    let b = u128::from(*other);
                    let mask = v6_mask(*prefix);
                    a & mask == b & mask
                }
                IpAddr::V4(_) => false,
            },
        }
    }
}

fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

/// `::ffff:a.b.c.d`（IPv4-mapped IPv6）归一化为 IPv4，使双栈 peer 与
/// IPv4 规则可比；其余地址原样返回。
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

impl IpFilter {
    /// 解析失败返回 Err（含出错规则），调用方应 fail-fast，**绝不静默丢弃**
    pub fn from_rules(allow: &[String], deny: &[String]) -> Result<Self, String> {
        let parse_all = |rules: &[String], what: &str| -> Result<Vec<Cidr>, String> {
            rules
                .iter()
                .map(|s| Cidr::parse(s).map_err(|e| format!("{what}: {e}")))
                .collect()
        };
        Ok(Self {
            allow: parse_all(allow, "allow")?,
            deny: parse_all(deny, "deny")?,
        })
    }

    pub fn allows(&self, ip: &IpAddr) -> bool {
        let ip = normalize(*ip);
        if self.deny.iter().any(|c| c.matches(&ip)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|c| c.matches(&ip))
    }
}

pub(crate) async fn handle<S>(State(state): State<S>, req: Request, next: Next) -> Response
where
    S: HasConfig + Send + Sync + 'static,
{
    let server = &state.config().load().server;
    if !server.ip_filter.enabled {
        return next.run(req).await;
    }
    // 启动期已校验过（state.rs），此处 Err 属防御分支：fail-closed 500
    let filter = match IpFilter::from_rules(&server.ip_filter.allow, &server.ip_filter.deny) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "ip_filter rule re-parse failed at request time");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(crate::web::response::ApiResponse::error(
                    500,
                    "internal server error",
                )),
            )
                .into_response();
        }
    };
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let mode = crate::utils::client_ip::IpKeyMode::parse(&server.ip_key_mode)
        .unwrap_or(crate::utils::client_ip::IpKeyMode::PeerIp);
    let ip = match mode {
        crate::utils::client_ip::IpKeyMode::PeerIp => peer,
        crate::utils::client_ip::IpKeyMode::ProxyHeaders => {
            crate::utils::client_ip::resolve(req.headers(), peer)
        }
    };
    match ip {
        Some(ip) if filter.allows(&ip) => next.run(req).await,
        Some(_) => (
            StatusCode::FORBIDDEN,
            axum::Json(crate::web::response::ApiResponse::error(403, "forbidden")),
        )
            .into_response(),
        // 解析不出来源 IP：配置了白名单时 fail-closed（不能把"看不见的 IP"当放行）
        None => {
            if !server.ip_filter.allow.is_empty() {
                (
                    StatusCode::FORBIDDEN,
                    axum::Json(crate::web::response::ApiResponse::error(403, "forbidden")),
                )
                    .into_response()
            } else {
                next.run(req).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_matches() {
        let f = IpFilter::from_rules(&["10.0.0.0/8".into(), "192.168.1.7".into()], &[]).unwrap();
        assert!(f.allows(&"10.1.2.3".parse().unwrap()));
        assert!(f.allows(&"192.168.1.7".parse().unwrap()));
        assert!(!f.allows(&"192.168.1.8".parse().unwrap()));
        assert!(!f.allows(&"11.0.0.1".parse().unwrap()));
        assert!(IpFilter::from_rules(&[], &[]).unwrap().allows(&"1.2.3.4".parse().unwrap()));

        let d = IpFilter::from_rules(&[], &["10.0.0.0/8".into()]).unwrap();
        assert!(!d.allows(&"10.1.2.3".parse().unwrap()));
    }

    #[test]
    fn ipv6_cidr_parses_and_matches() {
        let f = IpFilter::from_rules(&["2001:db8::/32".into(), "10.0.0.0/8".into()], &[]).unwrap();
        assert_eq!(f.allow.len(), 2, "IPv6 CIDR 不得被静默丢弃");
        assert!(f.allows(&"2001:db8:1234::1".parse().unwrap()));
        assert!(!f.allows(&"2001:db9::1".parse().unwrap()));
        // ::ffff:10.1.2.3 是 mapped IPv4，应命中 10.0.0.0/8
        assert!(f.allows(&"::ffff:10.1.2.3".parse().unwrap()));
        assert!(!f.allows(&"::ffff:11.0.0.1".parse().unwrap()));
    }

    #[test]
    fn invalid_rule_is_an_error_not_silently_dropped() {
        assert!(IpFilter::from_rules(&["not-an-ip".into()], &[]).is_err());
        assert!(IpFilter::from_rules(&["2001:db8::/129".into()], &[]).is_err());
        assert!(IpFilter::from_rules(&[], &["10.0.0.0/33".into()]).is_err());
    }

    #[test]
    fn only_ipv6_allowlist_blocks_public_ipv4() {
        // P0-2 回归：allow 只配 IPv6 时，IPv4 公网地址不得放行
        let f = IpFilter::from_rules(&["2001:db8::/32".into()], &[]).unwrap();
        assert!(!f.allows(&"8.8.8.8".parse().unwrap()));
    }
}
