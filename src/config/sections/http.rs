//! `[http]` 配置段（中间件开关集中于此）。
//!
//! 字段定义见 `docs/08` §3 `[http]`：中间件开关**统一置于此段**，不再为每个
//! 中间件单列 section。各字段按 feature 门控（未启用的 feature 忽略对应字段）。

use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::de::de_duration;

/// HTTP 中间件配置段。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
#[non_exhaustive]
pub struct HttpSection {
    /// 访问日志（feature `access-log`）。
    pub access_log: ToggleSection,
    /// 安全响应头（feature `security-headers`）。
    pub security_headers: SecurityHeadersSection,
    /// 响应压缩（**默认关闭**）。
    ///
    /// 全局压缩 + "响应里既有机密、又有攻击者可控的输入" = BREACH（逐字节还原机密）。
    /// 基座无法判断某个应用是否踩中，故默认关闭；确需压缩请显式
    /// `[http.compression] enabled = true`，并确认响应里不混合机密与回显输入。
    pub compression: DefaultOffToggle,
    /// IP 黑白名单（feature `ip-filter`）。
    #[cfg(feature = "ip-filter")]
    pub ip_filter: IpFilterSection,
    /// CSRF 双提交 Cookie（feature `csrf`）。
    #[cfg(feature = "csrf")]
    pub csrf: CsrfSection,
    /// 幂等性（feature `idempotency`）。
    #[cfg(feature = "idempotency")]
    pub idempotency: IdempotencySection,
    /// 语言协商中间件（feature `i18n`）。
    #[cfg(feature = "i18n")]
    pub locale: ToggleSection,
}

impl HttpSection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// 开关与配套参数不自洽时返回人类可读的错误信息。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// `production = true` 时额外要求 CSRF Cookie 带 `Secure`——它承载的是
    /// 一次性校验值，明文信道可被截获。
    ///
    /// # Errors
    /// 开关与配套参数不自洽时返回人类可读的错误信息。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        // `csrf` 关闭时 `production` 没有使用者；保留该参数是为了让签名与其它 section 一致
        let _ = production;
        #[cfg(feature = "csrf")]
        self.csrf.validate_for_env(production)?;
        #[cfg(feature = "idempotency")]
        self.idempotency.validate()?;
        #[cfg(feature = "ip-filter")]
        self.ip_filter.validate()?;
        Ok(())
    }
}

/// 安全响应头配置（`[http].security_headers`）。
///
/// 默认全开：`nosniff` / `X-Frame-Options` / `Referrer-Policy` 恒定注入，
/// `Cache-Control: no-store` 与 HSTS 按下面的开关控制。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct SecurityHeadersSection {
    /// 是否注入安全响应头（总开关）。
    pub enabled: bool,
    /// 是否注入 `Strict-Transport-Security`。
    ///
    /// **只在确认本次请求走的是 HTTPS 时才输出**（应用内 TLS，或对端是可信代理
    /// 且 `X-Forwarded-Proto: https`）：按 RFC 6797，浏览器会忽略明文 HTTP 上的
    /// HSTS 头，无脑输出只会造成「看起来配了、其实没生效」的错觉。
    pub hsts: bool,
    /// HSTS 有效期（`max-age`）。
    #[serde(deserialize_with = "de_duration")]
    pub hsts_max_age: Duration,
    /// HSTS 是否覆盖子域（`includeSubDomains`）。
    pub hsts_include_subdomains: bool,
    /// HSTS 是否申请浏览器预加载列表（`preload`）。
    ///
    /// 默认关闭：一旦被主流浏览器收录，撤出流程以月计，且要求覆盖全站子域。
    pub hsts_preload: bool,
    /// 是否给响应加 `Cache-Control: no-store`。
    ///
    /// API 响应常含 PII / 令牌，默认禁止任何缓存（浏览器 / 中间代理）。
    /// 需要缓存的路由可在 handler 里显式设置 `Cache-Control`，本中间件
    /// 用 `entry().or_insert()`，不会覆盖业务自己的值。
    pub no_store: bool,
}

impl Default for SecurityHeadersSection {
    fn default() -> Self {
        Self {
            enabled: true,
            hsts: true,
            hsts_max_age: Duration::from_secs(365 * 24 * 60 * 60),
            hsts_include_subdomains: true,
            hsts_preload: false,
            no_store: true,
        }
    }
}

/// 通用开关段（默认启用）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ToggleSection {
    /// 是否启用。
    pub enabled: bool,
}

impl Default for ToggleSection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 默认**关闭**的开关段（用于有副作用的层）。
///
/// 目前只有响应压缩：全局压缩 + "响应里既有机密又有攻击者可控输入" = BREACH，
/// 而基座无法判断某个应用是否踩中。默认关闭，由应用显式开启。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct DefaultOffToggle {
    /// 是否启用（`bool` 的默认值就是 `false`）。
    pub enabled: bool,
}

/// IP 过滤模式（`docs/08` §3 `[http].ip_filter.mode`）。
#[cfg(feature = "ip-filter")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IpFilterMode {
    /// 关闭（默认）。
    #[default]
    Off,
    /// 白名单：仅 `cidrs` 内的 IP 放行。
    Allow,
    /// 黑名单：`cidrs` 内的 IP 拒绝。
    Deny,
}

#[cfg(feature = "ip-filter")]
impl IpFilterMode {
    /// 配置中的小写名（用于错误信息）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// IP 黑白名单配置。
#[cfg(feature = "ip-filter")]
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct IpFilterSection {
    /// 过滤模式。
    pub mode: IpFilterMode,
    /// 网段列表（CIDR）；非法网段在**配置加载期**即报错。
    pub cidrs: Vec<ipnet::IpNet>,
}

#[cfg(feature = "ip-filter")]
impl IpFilterSection {
    /// 校验：启用过滤时 `cidrs` 不能为空。
    ///
    /// # Errors
    /// `mode != off` 且 `cidrs` 为空时返回错误（`allow` 会拒绝所有请求，
    /// `deny` 则是空操作，二者几乎都是误配）。
    pub fn validate(&self) -> Result<(), String> {
        if self.mode != IpFilterMode::Off && self.cidrs.is_empty() {
            return Err(format!(
                "[http].ip_filter.mode = \"{}\" 但 cidrs 为空；请配置网段或改为 \"off\"",
                self.mode.as_str()
            ));
        }
        Ok(())
    }

    /// IP 是否被规则拒绝。
    #[must_use]
    pub fn blocks(&self, ip: Option<std::net::IpAddr>) -> bool {
        match self.mode {
            IpFilterMode::Off => false,
            // 无法确定 IP 时白名单按拒绝处理（fail-closed）。
            IpFilterMode::Allow => !ip.is_some_and(|ip| self.contains(&ip)),
            IpFilterMode::Deny => ip.is_some_and(|ip| self.contains(&ip)),
        }
    }

    fn contains(&self, ip: &std::net::IpAddr) -> bool {
        self.cidrs.iter().any(|net| net.contains(ip))
    }
}

/// CSRF 双提交 Cookie 配置。
#[cfg(feature = "csrf")]
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct CsrfSection {
    /// 是否启用（默认关闭：纯 Bearer API 不需要 CSRF）。
    pub enabled: bool,
    /// 双提交 Cookie 名。
    pub cookie_name: String,
    /// Cookie 是否带 `Secure`（默认开启）。
    ///
    /// 该 Cookie **不能**设 `HttpOnly`（前端必须读出来放进请求头），所以它更容易
    /// 被脚本或明文信道拿走；`Secure` 至少保证它不会在 HTTP 上传输。
    /// 生产环境配 `false` 直接拒绝启动。
    pub secure: bool,
    /// token 有效期（`iat` + TTL）。过期后安全请求会自动换发。
    #[serde(deserialize_with = "de_duration")]
    pub ttl: Duration,
    /// 签名密钥（敏感项 🔒，只走环境变量 `AURORA_HTTP__CSRF__SECRET`）。
    ///
    /// 留空 → 每进程随机（**不跨实例**，启动期告警）。多实例部署必须配置同一值。
    pub secret: String,
    /// 会话 Cookie 名：token 会绑到它的值上，会话轮换后旧 token 自动失效。
    pub session_cookie: String,
}

#[cfg(feature = "csrf")]
impl Default for CsrfSection {
    fn default() -> Self {
        Self {
            enabled: false,
            cookie_name: "csrf_token".to_string(),
            secure: true,
            ttl: Duration::from_secs(2 * 60 * 60),
            secret: String::new(),
            session_cookie: "sid".to_string(),
        }
    }
}

#[cfg(feature = "csrf")]
impl CsrfSection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// Cookie 名非法时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// # Errors
    /// 名称不是合法 cookie-name（`token`，见 RFC 6265 §4.1.1），或生产环境
    /// 关闭了 `secure` 时返回错误。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let name = self.cookie_name.trim();
        if name.is_empty() {
            return Err("[http].csrf.cookie_name 不能为空".to_string());
        }
        if !name.bytes().all(is_cookie_name_byte) {
            return Err(format!(
                "[http].csrf.cookie_name = \"{name}\" 含非法字符（cookie 名只允许可见 ASCII，且不含分隔符）"
            ));
        }
        if self.ttl.is_zero() {
            return Err("[http].csrf.ttl 必须大于 0（否则 token 一签发就过期）".to_string());
        }
        // 签名密钥：非空则必须足够长（HMAC 密钥短于 32 字节等于把签名强度降到口令级）
        let secret = self.secret.trim();
        if !secret.is_empty() && secret.len() < 32 {
            return Err(format!(
                "[http].csrf.secret 只有 {} 字节，至少需要 32 字节",
                secret.len()
            ));
        }
        let session_cookie = self.session_cookie.trim();
        if !session_cookie.is_empty() && !session_cookie.bytes().all(is_cookie_name_byte) {
            return Err(format!(
                "[http].csrf.session_cookie = \"{session_cookie}\" 含非法字符（cookie 名只允许可见 ASCII）"
            ));
        }
        if name.starts_with("__Host-") && !self.secure {
            return Err(
                "[http].csrf.cookie_name 使用 `__Host-` 前缀时必须 secure = true".to_string(),
            );
        }
        if !self.secure {
            if production {
                return Err("[http].csrf.secure = false 在生产环境被拒绝：\
                     CSRF token 会以明文 HTTP 传输"
                    .to_string());
            }
            tracing::warn!("[http].csrf.secure = false：CSRF token 可被明文 HTTP 截获");
        }
        Ok(())
    }
}

/// RFC 6265 `cookie-name = token` 允许的字节。
#[cfg(feature = "csrf")]
fn is_cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&byte)
}

/// 幂等性配置。
#[cfg(feature = "idempotency")]
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct IdempotencySection {
    /// 是否启用（默认关闭）。
    pub enabled: bool,
    /// 幂等键保留时长。
    #[serde(deserialize_with = "de_duration")]
    pub ttl: Duration,
    /// 幂等键请求头名。
    pub header: String,
    /// 缓存故障时**拒绝**而非放行的路由（`"POST /api/v1/orders"`，可带尾部 `*`）。
    ///
    /// 与 [`default_fail_closed`](Self::default_fail_closed) 是"叠加"关系：
    /// 全局默认 + 路由级例外。当前者已为 `true` 时本字段没有额外作用。
    pub fail_closed_routes: Vec<String>,
    /// 缓存不可用时**全局**是否拒绝写请求（默认 `true`）。
    ///
    /// 默认 `true`（fail-closed）：幂等记录读不到时，返回 `50005` 而不是
    /// "降级为不校验"——后者会让同一 `Idempotency-Key` 的写请求重复执行
    /// （订单重复下单、支付重复扣款）。
    ///
    /// 显式设为 `false` 才回到 fail-open，且启动期会打 `warn!` 留痕。
    pub default_fail_closed: bool,
}

#[cfg(feature = "idempotency")]
impl Default for IdempotencySection {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl: Duration::from_secs(24 * 60 * 60),
            header: "Idempotency-Key".to_string(),
            fail_closed_routes: Vec::new(),
            default_fail_closed: true,
        }
    }
}

#[cfg(feature = "idempotency")]
impl IdempotencySection {
    /// 校验请求头名、TTL 与 fail-closed 路由声明。
    ///
    /// # Errors
    /// 请求头名非法、TTL 为 0、路由声明格式错误，或在未启用 `auth` 时声明
    /// fail-closed 路由（无主体可绑定）时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        http::header::HeaderName::from_bytes(self.header.trim().as_bytes()).map_err(|_| {
            format!(
                "[http].idempotency.header = \"{}\" 不是合法的请求头名",
                self.header
            )
        })?;
        if self.ttl.is_zero() {
            return Err("[http].idempotency.ttl 必须大于 0".to_string());
        }
        for raw in &self.fail_closed_routes {
            crate::http::middleware::idempotency::validate_route_pattern(raw)
                .map_err(|error| format!("[http].idempotency.fail_closed_routes: {error}"))?;
        }
        if !self.fail_closed_routes.is_empty() && !cfg!(feature = "auth") {
            return Err(
                "[http].idempotency.fail_closed_routes 需要启用 `auth` feature\
                 （fail-closed 依赖已认证主体生成幂等键）"
                    .to_string(),
            );
        }
        // fail-closed 同样依赖"已认证主体"：没有 auth 时幂等键的 scope 无从绑定，
        // 未认证请求根本不进幂等路径（见 idempotency::handle）。
        if self.default_fail_closed && !cfg!(feature = "auth") {
            return Err(
                "[http].idempotency.default_fail_closed = true 需要启用 `auth` feature\
                 （幂等键必须绑定已认证主体，否则会跨用户回放响应）"
                    .to_string(),
            );
        }
        if !self.default_fail_closed {
            tracing::warn!(
                "[http].idempotency.default_fail_closed = false：缓存不可用时写请求会**重复执行**，\
                 请确认这不是资金 / 订单类路由"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_middlewares_to_enabled() {
        let s = HttpSection::default();
        assert!(s.access_log.enabled);
        assert!(s.security_headers.enabled);
    }

    #[test]
    fn should_allow_disabling_middleware() {
        let json = r#"{ "access_log": { "enabled": false } }"#;
        let s: HttpSection = serde_json::from_str(json).unwrap();
        assert!(!s.access_log.enabled);
        assert!(s.security_headers.enabled);
    }

    #[cfg(feature = "csrf")]
    #[test]
    fn should_default_csrf_to_disabled() {
        let s = CsrfSection::default();
        assert!(!s.enabled);
        assert_eq!(s.cookie_name, "csrf_token");
        assert!(s.secure, "CSRF token 默认必须带 Secure");
    }

    #[cfg(feature = "csrf")]
    #[test]
    fn should_reject_invalid_cookie_name() {
        let mut s = CsrfSection {
            enabled: true,
            cookie_name: "bad name".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_err(), "含空格应被拒绝");

        s.cookie_name = "ok_name".to_string();
        assert!(s.validate().is_ok());
    }

    #[cfg(feature = "csrf")]
    #[test]
    fn should_reject_insecure_csrf_cookie_in_production() {
        let s = CsrfSection {
            enabled: true,
            secure: false,
            ..Default::default()
        };
        // 非生产只告警
        assert!(s.validate().is_ok());
        // 生产直接拒绝
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("csrf.secure"), "实际：{error}");
    }

    #[cfg(feature = "idempotency")]
    #[test]
    fn should_parse_idempotency_ttl() {
        let s: IdempotencySection = serde_json::from_str(r#"{ "ttl": "30m" }"#).unwrap();
        assert_eq!(s.ttl, Duration::from_secs(30 * 60));
        assert_eq!(s.header, "Idempotency-Key");
    }

    #[cfg(feature = "idempotency")]
    #[test]
    fn should_reject_zero_ttl() {
        let s = IdempotencySection {
            enabled: true,
            ttl: Duration::ZERO,
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[cfg(feature = "ip-filter")]
    #[test]
    fn should_parse_cidrs_and_match() {
        let s: IpFilterSection = serde_json::from_str(
            r#"{ "mode": "allow", "cidrs": ["10.0.0.0/8", "2001:db8::/32"] }"#,
        )
        .unwrap();
        assert_eq!(s.mode, IpFilterMode::Allow);
        assert!(s.contains(&"10.1.2.3".parse().unwrap()));
        assert!(s.contains(&"2001:db8::1".parse().unwrap()));
        assert!(!s.contains(&"192.168.1.1".parse().unwrap()));
    }

    #[cfg(feature = "ip-filter")]
    #[test]
    fn should_reject_invalid_cidr_at_parse_time() {
        let parsed: Result<IpFilterSection, _> =
            serde_json::from_str(r#"{ "mode": "deny", "cidrs": ["10.0.0.0/99"] }"#);
        assert!(parsed.is_err(), "非法网段应在配置加载期报错");
    }

    #[cfg(feature = "ip-filter")]
    #[test]
    fn should_fail_closed_when_ip_unknown_in_allow_mode() {
        let s = IpFilterSection {
            mode: IpFilterMode::Allow,
            cidrs: vec!["10.0.0.0/8".parse().unwrap()],
        };
        assert!(s.blocks(None), "白名单模式下无法确定 IP → 拒绝");
        assert!(s.blocks(Some("192.168.1.1".parse().unwrap())));
        assert!(!s.blocks(Some("10.1.1.1".parse().unwrap())));
    }

    #[cfg(feature = "ip-filter")]
    #[test]
    fn should_reject_empty_cidrs_when_enabled() {
        let s = IpFilterSection {
            mode: IpFilterMode::Deny,
            cidrs: Vec::new(),
        };
        assert!(s.validate().is_err());
    }
}
