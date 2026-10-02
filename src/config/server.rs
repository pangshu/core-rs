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
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_methods() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_headers() -> Vec<String> {
    vec!["*".to_string()]
}

/// `[server.rate_limit]` 配置段（feature = "rate-limit"），按客户端 IP 的令牌桶
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_per_second")]
    pub per_second: u64,
    #[serde(default = "default_burst")]
    pub burst: u32,
    /// 限流 key 来源：
    /// `peer_ip`（默认，直连部署）；`proxy_headers`（部署在反代后，从
    /// X-Forwarded-For / X-Real-IP / Forwarded 头取真实 IP，否则全站共享代理 IP 一个桶）
    #[serde(default)]
    pub key: String,
}

fn default_per_second() -> u64 {
    10
}
fn default_burst() -> u32 {
    20
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            per_second: default_per_second(),
            burst: default_burst(),
            key: String::new(),
        }
    }
}

/// `[server.compression]` 配置段：响应体 gzip 压缩
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// `[server]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
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
    /// 优雅停机最长等待（秒），超时后强制退出；0 表示一直等到在途请求处理完
    #[serde(default)]
    pub shutdown_timeout_secs: u64,
    #[serde(default)]
    pub cors: CorsConfig,
    /// 响应体压缩（gzip，默认关闭）
    #[serde(default)]
    pub compression: CompressionConfig,
    /// 按 IP 限流（feature = "rate-limit"）
    #[cfg(feature = "rate-limit")]
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    /// 静态资源目录，配置后由 /static/* 提供服务（feature = "upload"）
    #[cfg(feature = "upload")]
    #[serde(default)]
    pub static_dir: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            request_timeout_secs: default_timeout_secs(),
            body_limit: default_body_limit(),
            shutdown_timeout_secs: 0,
            cors: CorsConfig::default(),
            compression: CompressionConfig::default(),
            #[cfg(feature = "rate-limit")]
            rate_limit: RateLimitConfig::default(),
            #[cfg(feature = "upload")]
            static_dir: None,
        }
    }
}

/// `[server.cors]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 允许的来源列表，`*` 或留空表示全部放行
    #[serde(default = "default_origins")]
    pub allow_origins: Vec<String>,
    /// 允许的 HTTP 方法，如 ["GET", "POST"]；`*` 表示全部
    #[serde(default = "default_methods")]
    pub allow_methods: Vec<String>,
    /// 允许的请求头，如 ["Authorization", "Content-Type"]；`*` 表示全部
    #[serde(default = "default_headers")]
    pub allow_headers: Vec<String>,
    /// 允许浏览器 JS 读取的响应头（如分页 Total 头）
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// 允许携带凭证（cookie / TLS 客户端证书）；开启时 allow_origins 必须为显式列表
    #[serde(default)]
    pub allow_credentials: bool,
    /// 预检结果缓存时间（秒）；0 表示不发送 Max-Age
    #[serde(default)]
    pub max_age_secs: u64,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_origins: default_origins(),
            allow_methods: default_methods(),
            allow_headers: default_headers(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age_secs: 0,
        }
    }
}
