//! 限流键提取（`per_ip` / `per_key`）。
//!
//! ## 为什么键必须来自「已验签」的身份
//!
//! 本层位于 `auth` **外侧**（匿名流量也必须计入配额，见 `http/middleware/stack.rs`），
//! 因此这里拿不到 [`Identity`](crate::auth::Identity)。而 `Authorization` /
//! `X-Api-Key` 这类头是**客户端完全可控**的字符串：
//!
//! > 每次请求换一个随机值 → 每次都是一个全新的计数桶 → 限流等于不存在。
//!
//! 所以维度只能落在两类值上：
//!
//! - `per_ip`：客户端 IP（对端可信时才解析代理头，见 [`ClientIp::from_parts`]）
//! - `per_key`：**已验签**主体（`sub`，哈希后入键）→ 无法验证时**回退到 IP**
//!
//! `X-Api-Key` 不再参与限流：框架没有 API-Key 认证器，该头没有对应的校验链路，
//! 用它做维度等于给攻击者一个「无限配额开关」。业务若确需按 API Key 限流，
//! 应先在自己的认证链路里把 Key 换成已验签主体，再走 `per_key`。
//!
//! ## 键空间只有三种形态
//!
//! `Keys` 把「这次请求能算出哪些键」显式化，避免各调用方各自猜测：
//!
//! | 形态 | `subject` | `ip` | 主键 |
//! |---|---|---|---|
//! | 已验签主体 | `Some("sub:…")` | 可能 `Some("ip:…")` | 主体 |
//! | 未验签 / `per_ip` | `None` | `Some("ip:…")` | IP |
//! | 来源不可知 | `None` | `None` | `UNKNOWN_KEY` |
//!
//! 最后一种是**部署或链路异常**的信号（没有 `ConnectInfo`），不能和真实 IP 共用
//! 配额——否则攻击者用满一个共享桶就能让所有「来源不可知」的请求（含探活）一起
//! 429，限流反而变成全站 DoS 的开关。该情形由中间件单独处理（见 `layer.rs`）。

use axum::http::{Extensions, HeaderMap};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::RateLimitStrategy;
use crate::utils::{ClientIp, ProxyTrust};

/// 无法识别来源时使用的键（中间件会把它路由到独立的兜底配额）。
pub(crate) const UNKNOWN_KEY: &str = "ip:unknown";

/// 本次请求可用的限流键。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Keys {
    /// 已验签主体（`sub:<hash>`），仅 `per_key` 且验签通过时存在。
    pub subject: Option<String>,
    /// 客户端 IP（`ip:<addr>`），来源不可知时为 `None`。
    pub ip: Option<String>,
}

impl Keys {
    /// 主键：主体优先，其次 IP，都没有则是 [`UNKNOWN_KEY`]。
    pub fn primary(&self) -> &str {
        self.subject
            .as_deref()
            .or(self.ip.as_deref())
            .unwrap_or(UNKNOWN_KEY)
    }

    /// 来源是否完全不可知（既无主体也无 IP）。
    pub fn is_unknown(&self) -> bool {
        self.subject.is_none() && self.ip.is_none()
    }
}

/// 按策略提取限流键。
///
/// `subject` 必须是**已验签**的主体标识：未认证、验签失败、空串一律传 `None`，
/// 此时 `per_key` 会回退到 IP 桶（fail-closed，不会产生新的计数桶）。
///
/// `hasher` 只对主体做哈希（见 [`KeyHasher`]）；IP 本身直接入键——它已经是
/// 归一化后的字面量，再做哈希只会让排障更难。
pub(crate) fn keys_for(
    headers: &HeaderMap,
    extensions: &Extensions,
    strategy: RateLimitStrategy,
    trust: ProxyTrust<'_>,
    subject: Option<&str>,
    hasher: &KeyHasher,
) -> Keys {
    let subject = (strategy == RateLimitStrategy::PerKey)
        .then(|| subject.map(str::trim).filter(|s| !s.is_empty()))
        .flatten()
        .map(|subject| format!("sub:{}", hasher.hash(subject)));

    let ip = ClientIp::from_parts(headers, extensions, trust)
        .0
        .map(|ip| format!("ip:{ip}"));

    Keys { subject, ip }
}

/// 主体标识 → 定长键的哈希器。
///
/// ## 为什么不是 `DefaultHasher`
///
/// 早先用的是 `std::collections::hash_map::DefaultHasher`（SipHash-1-3，**种子固定为
/// 0**）。三个后果都不小：
///
/// 1. **可反查**：哈希确定且可离线计算，`sub` 若是数字 ID / 邮箱这类低熵值，从
///    Redis 键或 debug 日志（中间件会打印键）就能反推用户身份。
/// 2. **可构造碰撞**：业务若允许用户自选 `sub`（用户名、租户 ID），攻击者能离线
///    算出与目标用户**同桶**的 `sub`，然后用自己的流量打满对方配额（跨租户 DoS）。
/// 3. **跨版本不稳定**：`std` 明确不保证 `DefaultHasher` 的稳定性，滚动升级换了
///    toolchain，全部键都会变——限流临时翻倍、旧键残留到窗口过期。
///
/// 现在改成 `HMAC-SHA256(salt, sub)` 取前 128 bit：salt 未配置时**每进程随机**
/// （攻击者无法预计算），配置了则跨实例一致（Redis 共享桶需要）。
pub(crate) struct KeyHasher {
    key: Vec<u8>,
    ephemeral: bool,
}

impl std::fmt::Debug for KeyHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 绝不把 salt 打出来
        f.debug_struct("KeyHasher")
            .field("salt", &"<redacted>")
            .finish()
    }
}

impl KeyHasher {
    /// `salt` 为空时生成每进程随机的 32 字节（多实例共享桶须显式配置）。
    pub fn new(salt: &str) -> Self {
        if salt.trim().is_empty() {
            let mut key = Vec::with_capacity(32);
            key.extend_from_slice(Uuid::new_v4().as_bytes());
            key.extend_from_slice(Uuid::new_v4().as_bytes());
            return Self {
                key,
                ephemeral: true,
            };
        }
        Self {
            key: salt.as_bytes().to_vec(),
            ephemeral: false,
        }
    }

    /// 是否使用了随机盐（用于启动期提示「多实例桶不共享」）。
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// 128 bit 定长键（32 位小写十六进制）。
    pub fn hash(&self, value: &str) -> String {
        // `new_from_slice` 对 HMAC 而言接受任意长度密钥，不会失败
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("HMAC 接受任意长度密钥");
        mac.update(value.as_bytes());
        let bytes = mac.finalize().into_bytes();
        // 128 bit 足够避免碰撞，又比全量 256 bit 少一半键长度
        bytes[..16]
            .iter()
            .fold(String::with_capacity(32), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use axum::extract::ConnectInfo;
    use ipnet::IpNet;

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

    fn trusted() -> Vec<IpNet> {
        vec!["10.0.0.0/8".parse().unwrap()]
    }

    fn peer(ip: [u8; 4]) -> Extensions {
        let mut extensions = Extensions::new();
        extensions.insert(ConnectInfo(SocketAddr::from((ip, 4242))));
        extensions
    }

    fn hasher() -> KeyHasher {
        KeyHasher::new("test-salt")
    }

    #[test]
    fn should_key_by_ip_from_connect_info() {
        let keys = keys_for(
            &HeaderMap::new(),
            &peer([203, 0, 113, 9]),
            RateLimitStrategy::PerIp,
            ProxyTrust::disabled(),
            None,
            &hasher(),
        );
        assert_eq!(keys.primary(), "ip:203.0.113.9");
        assert!(keys.subject.is_none());
    }

    #[test]
    fn should_prefer_forwarded_for_when_trusted() {
        let headers = headers(&[("x-forwarded-for", "198.51.100.7, 10.0.0.1")]);
        let nets = trusted();
        let keys = keys_for(
            &headers,
            &peer([10, 0, 0, 1]),
            RateLimitStrategy::PerIp,
            ProxyTrust::new(true, &nets),
            None,
            &hasher(),
        );
        assert_eq!(keys.primary(), "ip:198.51.100.7");
    }

    #[test]
    fn should_ignore_forwarded_for_from_untrusted_peer() {
        // 直连对端不在可信网段 → 伪造的 XFF 被忽略，按对端限流
        let headers = headers(&[("x-forwarded-for", "8.8.8.8")]);
        let nets = trusted();
        let keys = keys_for(
            &headers,
            &peer([203, 0, 113, 9]),
            RateLimitStrategy::PerIp,
            ProxyTrust::new(true, &nets),
            None,
            &hasher(),
        );
        assert_eq!(keys.primary(), "ip:203.0.113.9");
    }

    #[test]
    fn should_report_unknown_without_peer_info() {
        // 无 ConnectInfo + trust_proxy：不采信转发头，来源判定为不可知
        let headers = headers(&[("x-forwarded-for", "203.0.113.7")]);
        let nets = trusted();
        let keys = keys_for(
            &headers,
            &Extensions::new(),
            RateLimitStrategy::PerIp,
            ProxyTrust::new(true, &nets),
            None,
            &hasher(),
        );
        assert_eq!(keys.primary(), UNKNOWN_KEY);
        assert!(keys.is_unknown());
    }

    #[test]
    fn should_key_by_verified_subject() {
        let extensions = peer([203, 0, 113, 9]);
        let keys = keys_for(
            &HeaderMap::new(),
            &extensions,
            RateLimitStrategy::PerKey,
            ProxyTrust::disabled(),
            Some("alice"),
            &hasher(),
        );

        let primary = keys.primary();
        assert!(primary.starts_with("sub:"), "实际：{primary}");
        assert!(!primary.contains("alice"), "不应把明文主体放进键");
        assert_eq!(primary.len(), "sub:".len() + 32);
        // IP 桶同时可用（`[ratelimit].ip_limit` 打开时叠加）
        assert_eq!(keys.ip.as_deref(), Some("ip:203.0.113.9"));

        // 同一主体 → 同一桶（换令牌不影响配额）
        let again = keys_for(
            &HeaderMap::new(),
            &extensions,
            RateLimitStrategy::PerKey,
            ProxyTrust::disabled(),
            Some("alice"),
            &hasher(),
        );
        assert_eq!(keys, again);

        // 不同主体 → 不同桶
        let bob = keys_for(
            &HeaderMap::new(),
            &extensions,
            RateLimitStrategy::PerKey,
            ProxyTrust::disabled(),
            Some("bob"),
            &hasher(),
        );
        assert_ne!(keys.subject, bob.subject);
    }

    /// 回归：客户端自带的凭据头**不得**成为计数桶。
    ///
    /// 修复前 `per_key` 直接哈希 `X-Api-Key` / `Authorization`，攻击者每请求换一个
    /// 随机值就能拿到无限配额；现在没有已验签主体时一律回退到 IP 桶。
    #[test]
    fn should_ignore_client_supplied_credentials_when_unverified() {
        let extensions = peer([203, 0, 113, 9]);

        let spoofed = headers(&[
            ("x-api-key", "random-1"),
            ("authorization", "Bearer random-2"),
        ]);
        let keys = keys_for(
            &spoofed,
            &extensions,
            RateLimitStrategy::PerKey,
            ProxyTrust::disabled(),
            None,
            &hasher(),
        );
        assert_eq!(
            keys.primary(),
            "ip:203.0.113.9",
            "未验签的凭据不得改变计数桶"
        );

        // 换一组头，桶不变 → 无法通过轮换头来放大配额
        let spoofed_again = headers(&[
            ("x-api-key", "random-3"),
            ("authorization", "Bearer random-4"),
        ]);
        assert_eq!(
            keys_for(
                &spoofed_again,
                &extensions,
                RateLimitStrategy::PerKey,
                ProxyTrust::disabled(),
                None,
                &hasher(),
            ),
            keys
        );
    }

    #[test]
    fn should_treat_blank_subject_as_unauthenticated() {
        let extensions = peer([203, 0, 113, 9]);

        for subject in [Some(""), Some("   ")] {
            let keys = keys_for(
                &HeaderMap::new(),
                &extensions,
                RateLimitStrategy::PerKey,
                ProxyTrust::disabled(),
                subject,
                &hasher(),
            );
            assert_eq!(keys.primary(), "ip:203.0.113.9");
        }
    }

    /// salt 必须真正参与哈希：换盐 → 换桶（否则「随机盐」只是摆设）。
    #[test]
    fn should_mix_salt_into_subject_key() {
        let a = KeyHasher::new("salt-a").hash("alice");
        let b = KeyHasher::new("salt-b").hash("alice");
        assert_ne!(a, b);
        // 同一盐 → 稳定
        assert_eq!(a, KeyHasher::new("salt-a").hash("alice"));
        // 未配置 → 每实例随机
        assert_ne!(
            KeyHasher::new("").hash("alice"),
            KeyHasher::new("").hash("alice")
        );
    }

    #[test]
    fn should_not_leak_plaintext_subject_into_key() {
        let hashed = KeyHasher::new("s").hash("alice@example.com");
        assert!(!hashed.contains("alice"));
        assert_eq!(hashed.len(), 32);
    }
}
