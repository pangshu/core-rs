//! `[server]` 配置节：监听地址、超时、body 上限、CORS、安全头、限流、幂等、
//! CSRF、IP 过滤、压缩与热更新开关。

use serde::{Deserialize, Serialize};

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_timeout_secs() -> u64 {
    30
}
fn default_body_limit() -> usize {
    2 * 1024 * 1024 // 2 MB
}
fn default_true() -> bool {
    true
}

/// `[server]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 单请求处理超时（秒），超时返回 408
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
    /// 请求体大小上限（字节）
    #[serde(default = "default_body_limit")]
    pub body_limit: usize,
    /// 优雅停机最长等待（秒），超时后强制退出；0 表示一直等在途请求
    #[serde(default)]
    pub shutdown_timeout_secs: u64,
    /// 客户端 IP 识别模式：直连用 peer 地址，反代后从请求头取
    #[serde(default)]
    pub ip_key_mode: String,
    #[serde(default)]
    pub cors: CorsSettings,
    #[serde(default)]
    pub security_headers: SecurityHeadersSettings,
    #[serde(default)]
    pub ip_filter: IpFilterSettings,
    /// 限流（feature = "rate-limit"）；阈值可热更新（文档 三·4）
    #[serde(default)]
    pub rate_limit: RateLimitSettings,
    /// 写接口幂等（文档 三·10）
    #[serde(default)]
    pub idempotency: IdempotencySettings,
    /// CSRF（feature = "csrf"）
    #[serde(default)]
    pub csrf: CsrfSettings,
    /// 响应体 gzip 压缩
    #[serde(default)]
    pub compression: FileBodySettings,
    /// /metrics 端点开关（feature = "metrics"）
    #[serde(default)]
    pub metrics: MetricsSettings,
    /// 配置热更新开关（文档 三·4）
    #[serde(default)]
    pub watch: WatchSettings,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            request_timeout_secs: default_timeout_secs(),
            body_limit: default_body_limit(),
            shutdown_timeout_secs: 0,
            ip_key_mode: String::new(),
            cors: CorsSettings::default(),
            security_headers: SecurityHeadersSettings::default(),
            ip_filter: IpFilterSettings::default(),
            rate_limit: RateLimitSettings::default(),
            idempotency: IdempotencySettings::default(),
            csrf: CsrfSettings::default(),
            compression: FileBodySettings::default(),
            metrics: MetricsSettings::default(),
            watch: WatchSettings::default(),
        }
    }
}

/// `[server.metrics]`：`/metrics` 端点开关。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSettings {
    /// 暴露 `/metrics`。**默认关闭**：全量内部指标（路由、耗时直方图、计数）
    /// 属于敏感运维信息，开启后请置于内网监听或自行叠加访问控制。
    #[serde(default)]
    pub enabled: bool,
}


/// `[server.cors]`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 允许的来源列表，`*` 或留空表示全部放行
    #[serde(default = "default_any")]
    pub allow_origins: Vec<String>,
    /// 允许的 HTTP 方法，如 ["GET", "POST"]；`*` 表示全部
    #[serde(default = "default_any")]
    pub allow_methods: Vec<String>,
    /// 允许的请求头；`*` 表示全部
    #[serde(default = "default_any")]
    pub allow_headers: Vec<String>,
    /// 允许浏览器 JS 读取的响应头
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// 允许携带凭证；开启时 allow_origins 必须为显式列表
    #[serde(default)]
    pub allow_credentials: bool,
    /// 预检结果缓存时间（秒）；0 表示不发送 Max-Age
    #[serde(default)]
    pub max_age_secs: u64,
}

fn default_any() -> Vec<String> {
    vec!["*".to_string()]
}

impl Default for CorsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_origins: default_any(),
            allow_methods: default_any(),
            allow_headers: default_any(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age_secs: 0,
        }
    }
}

/// `[server.security_headers]`：HSTS / X-Content-Type-Options / X-Frame-Options / CSP
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityHeadersSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// HSTS 值；空字符串不发送（仅 HTTPS 部署才应开启）
    #[serde(default)]
    pub hsts: String,
    /// X-Frame-Options；空字符串不发送
    #[serde(default = "default_frame_options")]
    pub frame_options: String,
    /// Content-Security-Policy；空字符串不发送
    #[serde(default)]
    pub content_security_policy: String,
    /// Referrer-Policy；空字符串不发送
    #[serde(default = "default_referrer")]
    pub referrer_policy: String,
}

fn default_frame_options() -> String {
    "SAMEORIGIN".to_string()
}
fn default_referrer() -> String {
    "strict-origin-when-cross-origin".to_string()
}

impl Default for SecurityHeadersSettings {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            hsts: String::new(),
            frame_options: default_frame_options(),
            content_security_policy: String::new(),
            referrer_policy: default_referrer(),
        }
    }
}

/// `[server.ip_filter]`：IPv4/IPv6 CIDR 黑白名单。allow 非空时仅放行列表内来源。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IpFilterSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// `[server.rate_limit]`（feature = "rate-limit"）：按客户端 IP 的固定窗口限流。
/// 阈值经 ConfigHandle 读取，**支持热更新**（文档 三·4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 窗口内允许的请求数
    #[serde(default = "default_rl_limit")]
    pub limit: u64,
    /// 窗口时长（秒）
    #[serde(default = "default_rl_window")]
    pub window_secs: u64,
    /// 限流 key 的 bucket 名（同一应用多套阈值时区分）
    #[serde(default)]
    pub bucket: String,
}

fn default_rl_limit() -> u64 {
    100
}
fn default_rl_window() -> u64 {
    60
}

impl Default for RateLimitSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            limit: default_rl_limit(),
            window_secs: default_rl_window(),
            bucket: String::new(),
        }
    }
}

/// `[server.idempotency]`：写接口防重（文档 三·10），底层用 cache 锁 + 响应回放。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencySettings {
    #[serde(default)]
    pub enabled: bool,
    /// 回放缓存 TTL（秒）
    #[serde(default = "default_idem_ttl")]
    pub ttl_secs: u64,
    /// 回放响应体的最大字节数（超过不缓存，只做占位防重）
    #[serde(default = "default_idem_body")]
    pub max_body_bytes: usize,
}

fn default_idem_ttl() -> u64 {
    86400
}
fn default_idem_body() -> usize {
    64 * 1024
}

impl Default for IdempotencySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: default_idem_ttl(),
            max_body_bytes: default_idem_body(),
        }
    }
}

/// `[server.csrf]`（feature = "csrf"）：双提交 Cookie 校验。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CsrfSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_csrf_cookie")]
    pub cookie_name: String,
    #[serde(default = "default_csrf_header")]
    pub header_name: String,
    /// 校验失败的响应码：403（默认）
    #[serde(default)]
    pub exempt_methods: Vec<String>,
}

fn default_csrf_cookie() -> String {
    "csrf_token".to_string()
}
fn default_csrf_header() -> String {
    "x-csrf-token".to_string()
}

impl Default for CsrfSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            cookie_name: default_csrf_cookie(),
            header_name: default_csrf_header(),
            exempt_methods: vec!["GET".into(), "HEAD".into(), "OPTIONS".into()],
        }
    }
}

/// `[server.compression]` / `[server.idempotency.body]` 等简单开关共用的形状
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileBodySettings {
    #[serde(default)]
    pub enabled: bool,
}


/// `[server.watch]`：配置热更新（feature = "watch"，默认开启）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchSettings {
    /// 是否启用配置文件监听；生产可关闭
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 去抖窗口（毫秒），合并编辑器连发事件
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_debounce_ms() -> u64 {
    300
}

impl Default for WatchSettings {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            debounce_ms: default_debounce_ms(),
        }
    }
}
