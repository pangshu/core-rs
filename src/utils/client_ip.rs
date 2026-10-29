//! 客户端 IP 提取。
//!
//! **只有直连对端落在 `[server].trusted_proxies` 内时**才解析 `X-Forwarded-For` /
//! `X-Real-IP`；否则一律用 TCP 对端地址。否则「直连客户端自带一个 XFF 头」就能
//! 伪造来源，绕过 `ip_filter` 白名单与按 IP 限流，并污染审计日志 / geoip。
//!
//! 解析顺序（对端可信时）：`X-Forwarded-For` **从右往左**跳过可信代理，取第一个
//! 不可信地址 → `X-Real-IP` → TCP 对端。
//!
//! **拿不到 TCP 对端时 fail-closed**：请求扩展里没有 [`ConnectInfo`] 说明这条链路
//! 不是 `axum::serve` 建的（自建 server、`oneshot`、把 Router 嵌进别的服务、
//! Serverless）。此时**无法校验对端是否可信**，若仍按配置解析转发头，等于把
//! 「客户端完全可控的字符串」当成来源 IP：按 IP 限流可被逐请求换值绕过、
//! `ip_filter` 白名单可被伪造值穿透、geoip 与审计日志一并污染。
//! 因此这种情况一律返回 `None`（= 来源不可知），由各消费方按 fail-closed 处理。
//!
//! **归一化**：所有进入本模块的地址（TCP 对端 / 每个 XFF 跳 / `X-Real-IP`）先做
//! `::ffff:a.b.c.d` → `a.b.c.d`，再参与任何判断。双栈监听（`host = "::"`）下
//! 对端常是 v4-mapped 形态，不归一化会有两个后果：① 可信网段比对失配（
//! `10.0.0.0/8` 匹配不到 `::ffff:10.0.0.1`），代理反而被当成客户端；② 同一客户端
//! 在「直连」与「经代理」两条路径上得到两个不同的 `IpAddr`，geoip 查库、缓存键、
//! `ip_filter` 白名单、按 IP 限流四处判定互相矛盾。
//!
//! [`ClientIp`] 同时是 axum 提取器，可直接用于 handler 参数。

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap};
use ipnet::IpNet;

use crate::app::state::AppState;

/// 代理信任策略（`[server].trust_proxy` + `[server].trusted_proxies`）。
#[derive(Debug, Clone, Copy)]
pub struct ProxyTrust<'a> {
    enabled: bool,
    trusted: &'a [IpNet],
}

impl<'a> ProxyTrust<'a> {
    /// 由配置构造。
    #[must_use]
    pub fn new(enabled: bool, trusted: &'a [IpNet]) -> Self {
        Self { enabled, trusted }
    }

    /// 不信任任何代理（等价于 `trust_proxy = false`）。
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            trusted: &[],
        }
    }

    /// 是否启用代理头解析。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 该地址是否属于可信代理。
    ///
    /// 入参应已是归一化形态（`from_parts` 保证）。为兼容把网段写成 v4-mapped
    /// 形式（`::ffff:10.0.0.0/104`）的配置，IPv4 地址会再以 `::ffff:a.b.c.d`
    /// 比一次——否则归一化会让这类网段突然失配，代理被当成客户端。
    #[must_use]
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        if self.trusted.iter().any(|net| net.contains(&ip)) {
            return true;
        }
        match ip {
            IpAddr::V4(v4) => {
                let mapped = IpAddr::V6(v4.to_ipv6_mapped());
                self.trusted.iter().any(|net| net.contains(&mapped))
            }
            IpAddr::V6(_) => false,
        }
    }
}

/// 客户端 IP（可能为空，表示无法确定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 读**冻结的安全边界**（与中间件同源）：热更新不得放宽信任判定
        let boundary = state.config.boundary();
        let trust = ProxyTrust::new(boundary.trust_proxy, &boundary.trusted_proxies);
        Ok(Self::from_parts(&parts.headers, &parts.extensions, trust))
    }
}

impl ClientIp {
    /// 从请求头提取客户端 IP（只解析代理头，**不做对端可信判断**）。
    ///
    /// 生产路径请用 [`from_parts`](Self::from_parts)：它要求对端地址可信。
    #[must_use]
    pub fn from_headers(headers: &HeaderMap, trust: ProxyTrust<'_>) -> Self {
        if !trust.enabled {
            return Self(None);
        }
        Self(forwarded_ip(headers, trust))
    }

    /// 从请求头 + 连接信息提取客户端 IP。
    ///
    /// - `trust_proxy = false`：直接用 TCP 对端地址，忽略一切转发头；
    /// - 对端**不在** `trusted_proxies` 内：同样忽略转发头（防伪造）；
    /// - 对端可信：解析转发头，拿不到再回退对端；
    /// - **没有 `ConnectInfo`**（对端不可知）：返回 `None`，且**不解析转发头**。
    ///
    /// 返回值一律已归一化（见模块文档），可直接作为限流 / 审计 / geoip 的键。
    ///
    /// `ConnectInfo` 由 `boot::serve` 通过 `into_make_service_with_connect_info`
    /// 注入。用 `tower::ServiceExt::oneshot` 直接打 Router 时不存在，此时来源
    /// 判定为「不可知」——**生产必须走 `serve()`**，否则按 IP 的防护全部失效
    /// （`TestApp` 会自动补一个 `127.0.0.1` 对端，见 `testing::TestApp`）。
    #[must_use]
    pub fn from_parts(headers: &HeaderMap, extensions: &Extensions, trust: ProxyTrust<'_>) -> Self {
        // 先归一化再比对可信网段：v4-mapped 的对端否则匹配不上 `10.0.0.0/8`
        let peer = extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| canonical(info.0.ip()));

        if !trust.enabled {
            return Self(peer);
        }

        match peer {
            // 对端不是可信代理：它带来的转发头一律不采信
            Some(peer) if !trust.is_trusted(peer) => Self(Some(peer)),
            Some(peer) => Self(forwarded_ip(headers, trust).or(Some(peer))),
            // 对端不可知：无法判断它是不是可信代理 → 转发头一个字节都不采信
            None => {
                warn_missing_peer();
                Self(None)
            }
        }
    }
}

/// 「请求里没有 `ConnectInfo`」只在部署方式不对时出现，用一次性日志把它喊出来。
///
/// 每次请求都打会刷屏，故用 `Once`；`serve()` 正常时永远不会触发。
fn warn_missing_peer() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "请求缺少 ConnectInfo：无法判定 TCP 对端，按 IP 的限流 / IP 黑白名单 / geoip \
             将退化为「来源不可知」。生产环境请使用 `boot::serve`（它通过 \
             `into_make_service_with_connect_info` 注入对端地址）"
        );
    });
}

/// 归一化：`::ffff:a.b.c.d`（IPv4-mapped IPv6）→ `a.b.c.d`。
///
/// 只在地址**进入**本模块时做一次，之后所有比对与返回都基于归一化形态。
fn canonical(ip: IpAddr) -> IpAddr {
    ip.to_canonical()
}

/// 解析 `X-Forwarded-For` / `X-Real-IP`。
fn forwarded_ip(headers: &HeaderMap, trust: ProxyTrust<'_>) -> Option<IpAddr> {
    if let Some(list) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        // 每一跳都归一化后再判可信：否则 `::ffff:10.0.0.2` 不会被识别为可信代理，
        // 「从右往左跳过可信代理」会直接把代理自己当成客户端。
        let hops: Vec<IpAddr> = list
            .split(',')
            .filter_map(|part| part.trim().parse().ok().map(canonical))
            .collect();
        // 从右往左跳过可信代理：第一个不可信地址才是真实客户端
        if let Some(ip) = hops.iter().rev().find(|ip| !trust.is_trusted(**ip)) {
            return Some(*ip);
        }
        // 整条链路都是可信代理（或全是内网）：退回最左侧
        if let Some(first) = hops.first() {
            return Some(*first);
        }
    }
    real_ip(headers)
}

fn real_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-real-ip")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidrs(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|c| c.parse().unwrap()).collect()
    }

    fn extensions(peer: [u8; 4]) -> Extensions {
        let mut ext = Extensions::new();
        ext.insert(ConnectInfo(SocketAddr::from((peer, 4242))));
        ext
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn should_ignore_proxy_headers_when_untrusted() {
        let headers = headers(&[("x-forwarded-for", "203.0.113.7, 10.0.0.1")]);
        let trust = ProxyTrust::new(false, &[]);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("10.0.0.1".parse().unwrap())),
            "trust_proxy = false 时应只用 TCP 对端"
        );
    }

    #[test]
    fn should_ignore_forwarded_for_from_untrusted_peer() {
        // 直连客户端伪造 XFF：对端 203.0.113.9 不在可信网段 → 头被忽略
        let headers = headers(&[("x-forwarded-for", "8.8.8.8")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([203, 0, 113, 9]), trust),
            ClientIp(Some("203.0.113.9".parse().unwrap()))
        );
    }

    #[test]
    fn should_take_client_ip_when_peer_is_trusted_proxy() {
        let headers = headers(&[("x-forwarded-for", "198.51.100.7, 10.0.0.2")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("198.51.100.7".parse().unwrap()))
        );
    }

    #[test]
    fn should_skip_trusted_hops_from_the_right() {
        // 右侧 10.0.0.9 是内网代理 → 跳过，取 198.51.100.7
        let headers = headers(&[("x-forwarded-for", "198.51.100.7, 10.0.0.9")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("198.51.100.7".parse().unwrap()))
        );
    }

    #[test]
    fn should_fall_back_to_real_ip() {
        let headers = headers(&[("x-real-ip", "198.51.100.9")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("198.51.100.9".parse().unwrap()))
        );
    }

    #[test]
    fn should_fall_back_to_peer_when_headers_missing() {
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&HeaderMap::new(), &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("10.0.0.1".parse().unwrap()))
        );
    }

    #[test]
    fn should_fail_closed_when_peer_is_unknown() {
        // 无 ConnectInfo（oneshot / 自建 server / 嵌入别的服务）时无法校验对端，
        // 转发头一律不采信——否则客户端自带的 XFF 就能冒充任意来源 IP。
        let xff = headers(&[("x-forwarded-for", "203.0.113.7")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&xff, &Extensions::new(), trust),
            ClientIp(None),
            "对端不可知时不得采信 XFF"
        );

        // 连 X-Real-IP 也不采信
        let real_ip_only = headers(&[("x-real-ip", "203.0.113.7")]);
        assert_eq!(
            ClientIp::from_parts(&real_ip_only, &Extensions::new(), trust),
            ClientIp(None)
        );
    }

    #[test]
    fn should_canonicalize_ipv4_mapped_peer() {
        // 双栈监听（host = "::"）下对端是 ::ffff:a.b.c.d
        let mut ext = Extensions::new();
        ext.insert(ConnectInfo(SocketAddr::new(
            "::ffff:203.0.113.9".parse().unwrap(),
            4242,
        )));
        let trust = ProxyTrust::new(false, &[]);
        assert_eq!(
            ClientIp::from_parts(&HeaderMap::new(), &ext, trust),
            ClientIp(Some("203.0.113.9".parse().unwrap())),
            "v4-mapped 对端应归一化为 IPv4"
        );
    }

    #[test]
    fn should_match_trusted_proxy_when_peer_is_ipv4_mapped() {
        // 对端 ::ffff:10.0.0.1 必须命中 10.0.0.0/8，否则代理会被当成客户端
        let mut ext = Extensions::new();
        ext.insert(ConnectInfo(SocketAddr::new(
            "::ffff:10.0.0.1".parse().unwrap(),
            4242,
        )));
        let headers = headers(&[("x-forwarded-for", "198.51.100.7")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &ext, trust),
            ClientIp(Some("198.51.100.7".parse().unwrap())),
            "v4-mapped 的可信对端仍应解析转发头"
        );
    }

    #[test]
    fn should_skip_ipv4_mapped_trusted_hop() {
        // 右侧 ::ffff:10.0.0.9 是内网代理 → 归一化后应被识别为可信并跳过
        let headers = headers(&[("x-forwarded-for", "::ffff:198.51.100.7, ::ffff:10.0.0.9")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("198.51.100.7".parse().unwrap())),
            "v4-mapped 的可信跳应被跳过，而不是当成客户端"
        );
    }

    #[test]
    fn should_canonicalize_real_ip() {
        let headers = headers(&[("x-real-ip", "::ffff:198.51.100.9")]);
        let trusted = cidrs(&["10.0.0.0/8"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("198.51.100.9".parse().unwrap()))
        );
    }

    #[test]
    fn should_match_v4_mapped_trusted_cidr() {
        // 兼容把可信网段写成 v4-mapped 形式的配置
        let trusted = cidrs(&["::ffff:10.0.0.0/104"]);
        let trust = ProxyTrust::new(true, &trusted);
        assert!(trust.is_trusted("10.0.0.1".parse().unwrap()));
        assert!(!trust.is_trusted("203.0.113.9".parse().unwrap()));

        let headers = headers(&[("x-forwarded-for", "198.51.100.7")]);
        assert_eq!(
            ClientIp::from_parts(&headers, &extensions([10, 0, 0, 1]), trust),
            ClientIp(Some("198.51.100.7".parse().unwrap())),
            "v4-mapped 网段仍应识别出可信代理"
        );
    }
}
