//! `[server]` 配置节：监听地址、超时、body 上限、CORS、安全头、限流、幂等、
//! CSRF、IP 过滤、压缩与热更新开关。

use serde::{Deserialize, Serialize}; // 引入 serde 的序列化/反序列化派生宏，配置节需要

fn default_host() -> String { // serde 默认监听地址的取值函数
    "0.0.0.0".to_string() // 默认监听所有网卡
}
fn default_port() -> u16 { // serde 默认端口取值函数
    8080 // 默认端口 8080
}
fn default_timeout_secs() -> u64 { // serde 默认请求超时取值函数
    30 // 默认超时 30 秒
}
fn default_body_limit() -> usize { // serde 默认请求体上限取值函数
    2 * 1024 * 1024 // 2 MB
}
fn default_true() -> bool { // serde 默认布尔开关取值函数（默认开启）
    true // 默认值 true
}

/// `[server]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试、克隆与 serde 序列化/反序列化
pub struct ServerSettings { // 定义 `[server]` 配置段结构体
    #[serde(default = "default_host")] // 缺省时用 default_host 填充
    pub host: String, // 监听地址
    #[serde(default = "default_port")] // 缺省时用 default_port 填充
    pub port: u16, // 监听端口
    /// 单请求处理超时（秒），超时返回 408
    #[serde(default = "default_timeout_secs")] // 缺省时用 default_timeout_secs 填充
    pub request_timeout_secs: u64, // 单请求处理超时（秒）
    /// 请求体大小上限（字节）
    #[serde(default = "default_body_limit")] // 缺省时用 default_body_limit 填充
    pub body_limit: usize, // 请求体大小上限（字节）
    /// 优雅停机最长等待（秒），超时后强制退出；0 表示一直等在途请求
    #[serde(default)] // 缺省时用类型默认值（0）
    pub shutdown_timeout_secs: u64, // 优雅停机最长等待（秒）
    /// 客户端 IP 识别模式：直连用 peer 地址，反代后从请求头取
    #[serde(default)] // 缺省时用类型默认值（空串）
    pub ip_key_mode: String, // 客户端 IP 识别模式
    #[serde(default)] // 缺省时用 CorsSettings 默认值
    pub cors: CorsSettings, // 跨域配置
    #[serde(default)] // 缺省时用安全头默认值
    pub security_headers: SecurityHeadersSettings, // 安全响应头配置
    #[serde(default)] // 缺省时用 IP 过滤默认值
    pub ip_filter: IpFilterSettings, // IP 黑白名单配置
    /// 限流（feature = "rate-limit"）；阈值可热更新（文档 三·4）
    #[serde(default)] // 缺省时用限流默认值
    pub rate_limit: RateLimitSettings, // 限流配置
    /// 写接口幂等（文档 三·10）
    #[serde(default)] // 缺省时用幂等默认值
    pub idempotency: IdempotencySettings, // 幂等配置
    /// CSRF（feature = "csrf"）
    #[serde(default)] // 缺省时用 CSRF 默认值
    pub csrf: CsrfSettings, // CSRF 防护配置
    /// 响应体 gzip 压缩
    #[serde(default)] // 缺省时用压缩默认值
    pub compression: FileBodySettings, // 压缩开关配置
    /// /metrics 端点开关（feature = "metrics"）
    #[serde(default)] // 缺省时用 metrics 默认值
    pub metrics: MetricsSettings, // /metrics 端点配置
    /// 配置热更新开关（文档 三·4）
    #[serde(default)] // 缺省时用热更新默认值
    pub watch: WatchSettings, // 配置热更新配置
}

impl Default for ServerSettings { // 为 ServerSettings 手写默认值实现
    fn default() -> Self { // 实现 default 方法
        Self { // 逐字段构造默认实例
            host: default_host(), // 监听地址默认 0.0.0.0
            port: default_port(), // 端口默认 8080
            request_timeout_secs: default_timeout_secs(), // 超时默认 30 秒
            body_limit: default_body_limit(), // body 上限默认 2MB
            shutdown_timeout_secs: 0, // 停机等待默认 0（一直等在途请求）
            ip_key_mode: String::new(), // IP 识别模式默认空串
            cors: CorsSettings::default(), // CORS 默认配置
            security_headers: SecurityHeadersSettings::default(), // 安全头默认配置
            ip_filter: IpFilterSettings::default(), // IP 过滤默认配置
            rate_limit: RateLimitSettings::default(), // 限流默认配置
            idempotency: IdempotencySettings::default(), // 幂等默认配置
            csrf: CsrfSettings::default(), // CSRF 默认配置
            compression: FileBodySettings::default(), // 压缩默认配置
            metrics: MetricsSettings::default(), // metrics 默认配置
            watch: WatchSettings::default(), // 热更新默认配置
        }
    }
}

/// `[server.metrics]`：`/metrics` 端点开关。
#[derive(Debug, Clone, Default, Serialize, Deserialize)] // 派生调试/克隆/默认值与 serde
pub struct MetricsSettings { // 定义 `[server.metrics]` 配置
    /// 暴露 `/metrics`。**默认关闭**：全量内部指标（路由、耗时直方图、计数）
    /// 属于敏感运维信息，开启后请置于内网监听或自行叠加访问控制。
    #[serde(default)] // 缺省为 false（默认关闭）
    pub enabled: bool, // 是否暴露 /metrics
}


/// `[server.cors]`
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct CorsSettings { // 定义 `[server.cors]` 配置
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用 CORS
    /// 允许的来源列表，`*` 或留空表示全部放行
    #[serde(default = "default_any")] // 缺省为 ["*"]
    pub allow_origins: Vec<String>, // 允许的来源列表
    /// 允许的 HTTP 方法，如 ["GET", "POST"]；`*` 表示全部
    #[serde(default = "default_any")] // 缺省为 ["*"]
    pub allow_methods: Vec<String>, // 允许的 HTTP 方法
    /// 允许的请求头；`*` 表示全部
    #[serde(default = "default_any")] // 缺省为 ["*"]
    pub allow_headers: Vec<String>, // 允许的请求头
    /// 允许浏览器 JS 读取的响应头
    #[serde(default)] // 缺省为空列表
    pub expose_headers: Vec<String>, // 允许 JS 读取的响应头
    /// 允许携带凭证；开启时 allow_origins 必须为显式列表
    #[serde(default)] // 缺省为 false
    pub allow_credentials: bool, // 是否允许携带凭证
    /// 预检结果缓存时间（秒）；0 表示不发送 Max-Age
    #[serde(default)] // 缺省为 0
    pub max_age_secs: u64, // 预检缓存时间（秒）
}

fn default_any() -> Vec<String> { // CORS 列表默认值函数
    vec!["*".to_string()] // 默认放行全部（"*"）
}

impl Default for CorsSettings { // 为 CorsSettings 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: false, // 默认关闭
            allow_origins: default_any(), // 来源默认全放行
            allow_methods: default_any(), // 方法默认全放行
            allow_headers: default_any(), // 头默认全放行
            expose_headers: Vec::new(), // 无暴露头
            allow_credentials: false, // 默认不允许凭证
            max_age_secs: 0, // 默认不发送 Max-Age
        }
    }
}

/// `[server.security_headers]`：HSTS / X-Content-Type-Options / X-Frame-Options / CSP
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct SecurityHeadersSettings { // 定义 `[server.security_headers]` 配置
    #[serde(default = "default_true")] // 缺省为 true（默认开启）
    pub enabled: bool, // 是否启用安全头
    /// HSTS 值；空字符串不发送（仅 HTTPS 部署才应开启）
    #[serde(default)] // 缺省为空串（不发送）
    pub hsts: String, // HSTS 值
    /// X-Frame-Options；空字符串不发送
    #[serde(default = "default_frame_options")] // 缺省为 SAMEORIGIN
    pub frame_options: String, // X-Frame-Options 值
    /// Content-Security-Policy；空字符串不发送
    #[serde(default)] // 缺省为空串（不发送）
    pub content_security_policy: String, // CSP 值
    /// Referrer-Policy；空字符串不发送
    #[serde(default = "default_referrer")] // 缺省为 strict-origin-when-cross-origin
    pub referrer_policy: String, // Referrer-Policy 值
}

fn default_frame_options() -> String { // X-Frame-Options 默认值函数
    "SAMEORIGIN".to_string() // 默认同源可嵌入
}
fn default_referrer() -> String { // Referrer-Policy 默认值函数
    "strict-origin-when-cross-origin".to_string() // 默认跨源仅发来源
}

impl Default for SecurityHeadersSettings { // 为安全头配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: default_true(), // 默认开启
            hsts: String::new(), // 默认不发送 HSTS
            frame_options: default_frame_options(), // 默认 SAMEORIGIN
            content_security_policy: String::new(), // 默认不发送 CSP
            referrer_policy: default_referrer(), // 默认 strict-origin-when-cross-origin
        }
    }
}

/// `[server.ip_filter]`：IPv4/IPv6 CIDR 黑白名单。allow 非空时仅放行列表内来源。
#[derive(Debug, Clone, Serialize, Deserialize, Default)] // 派生调试/克隆/serde/默认值
pub struct IpFilterSettings { // 定义 `[server.ip_filter]` 配置
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用 IP 过滤
    #[serde(default)] // 缺省为空列表
    pub allow: Vec<String>, // 允许的 CIDR 列表
    #[serde(default)] // 缺省为空列表
    pub deny: Vec<String>, // 拒绝的 CIDR 列表
}

/// `[server.rate_limit]`（feature = "rate-limit"）：按客户端 IP 的固定窗口限流。
/// 阈值经 ConfigHandle 读取，**支持热更新**（文档 三·4）。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct RateLimitSettings { // 定义 `[server.rate_limit]` 配置
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用限流
    /// 窗口内允许的请求数
    #[serde(default = "default_rl_limit")] // 缺省为 100
    pub limit: u64, // 窗口内请求数上限
    /// 窗口时长（秒）
    #[serde(default = "default_rl_window")] // 缺省为 60
    pub window_secs: u64, // 窗口时长（秒）
    /// 限流 key 的 bucket 名（同一应用多套阈值时区分）
    #[serde(default)] // 缺省为空串
    pub bucket: String, // 限流 key 的 bucket 名
}

fn default_rl_limit() -> u64 { // 限流阈值默认值函数
    100 // 默认 100 次
}
fn default_rl_window() -> u64 { // 限流窗口默认值函数
    60 // 默认 60 秒
}

impl Default for RateLimitSettings { // 为限流配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: false, // 默认关闭
            limit: default_rl_limit(), // 默认 100
            window_secs: default_rl_window(), // 默认 60 秒
            bucket: String::new(), // 默认空 bucket
        }
    }
}

/// `[server.idempotency]`：写接口防重（文档 三·10），底层用 cache 锁 + 响应回放。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct IdempotencySettings { // 定义 `[server.idempotency]` 配置
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用幂等
    /// 回放缓存 TTL（秒）
    #[serde(default = "default_idem_ttl")] // 缺省为 86400
    pub ttl_secs: u64, // 回放缓存 TTL（秒）
    /// 回放响应体的最大字节数（超过不缓存，只做占位防重）
    #[serde(default = "default_idem_body")] // 缺省为 64KB
    pub max_body_bytes: usize, // 回放响应体最大字节数
}

fn default_idem_ttl() -> u64 { // 幂等 TTL 默认值函数
    86400 // 默认一天
}
fn default_idem_body() -> usize { // 幂等 body 上限默认值函数
    64 * 1024 // 默认 64KB
}

impl Default for IdempotencySettings { // 为幂等配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: false, // 默认关闭
            ttl_secs: default_idem_ttl(), // 默认 86400 秒
            max_body_bytes: default_idem_body(), // 默认 64KB
        }
    }
}

/// `[server.csrf]`（feature = "csrf"）：双提交 Cookie 校验。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct CsrfSettings { // 定义 `[server.csrf]` 配置
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用 CSRF 校验
    #[serde(default = "default_csrf_cookie")] // 缺省为 csrf_token
    pub cookie_name: String, // CSRF Cookie 名
    #[serde(default = "default_csrf_header")] // 缺省为 x-csrf-token
    pub header_name: String, // CSRF 请求头名
    /// 校验失败的响应码：403（默认）
    #[serde(default)] // 缺省为空列表
    pub exempt_methods: Vec<String>, // 免校验的 HTTP 方法
    /// 签名密钥（HMAC-SHA256）。**只走环境变量**（如 `APP_SERVER__CSRF__SECRET`），
    /// 不写入 toml；≥32 字节（启动 fail-fast 校验）。空 = 兼容模式（token 仅做
    /// 相等比较，不签名不绑会话）——建议始终配置。
    #[serde(default)] // 缺省为空串（兼容模式）
    pub secret: String, // HMAC 签名密钥
    /// 签名 token 有效期（秒）；0 = 不过期。会话切换（登录/登出）后应重新下发
    #[serde(default = "default_csrf_ttl")] // 缺省为 86400
    pub token_ttl_secs: u64, // token 有效期（秒）
    /// 可信 Origin 列表（如 `https://app.example.com`）。非空时，浏览器带来的
    /// `Origin` 头必须命中列表，否则 403；未带 Origin 的请求（非浏览器客户端）
    /// 跳过本校验，由签名 token 承担主防线。空 = 不校验 Origin。
    #[serde(default)] // 缺省为空列表
    pub allowed_origins: Vec<String>, // 可信 Origin 列表
}

fn default_csrf_cookie() -> String { // CSRF Cookie 名默认值函数
    "csrf_token".to_string() // 默认 cookie 名
}
fn default_csrf_header() -> String { // CSRF 头名默认值函数
    "x-csrf-token".to_string() // 默认头名
}
fn default_csrf_ttl() -> u64 { // CSRF token TTL 默认值函数
    86_400 // 默认一天
}

impl Default for CsrfSettings { // 为 CSRF 配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: false, // 默认关闭
            cookie_name: default_csrf_cookie(), // 默认 csrf_token
            header_name: default_csrf_header(), // 默认 x-csrf-token
            exempt_methods: vec!["GET".into(), "HEAD".into(), "OPTIONS".into()], // 默认豁免 GET/HEAD/OPTIONS
            secret: String::new(), // 默认空密钥
            token_ttl_secs: default_csrf_ttl(), // 默认 86400 秒
            allowed_origins: Vec::new(), // 默认不校验 Origin
        }
    }
}

/// `[server.compression]` / `[server.idempotency.body]` 等简单开关共用的形状
#[derive(Debug, Clone, Default, Serialize, Deserialize)] // 派生调试/克隆/默认值/serde
pub struct FileBodySettings { // 定义通用开关结构体
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用
}


/// `[server.watch]`：配置热更新（feature = "watch"，默认开启）。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct WatchSettings { // 定义 `[server.watch]` 配置
    /// 是否启用配置文件监听；生产可关闭
    #[serde(default = "default_true")] // 缺省为 true（默认开启）
    pub enabled: bool, // 是否启用配置监听
    /// 去抖窗口（毫秒），合并编辑器连发事件
    #[serde(default = "default_debounce_ms")] // 缺省为 300ms
    pub debounce_ms: u64, // 去抖窗口（毫秒）
}

fn default_debounce_ms() -> u64 { // 去抖默认值函数
    300 // 默认 300 毫秒
}

impl Default for WatchSettings { // 为热更新配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: default_true(), // 默认开启
            debounce_ms: default_debounce_ms(), // 默认 300 毫秒
        }
    }
}
