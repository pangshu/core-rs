//! 各子系统配置结构，集中定义于此（文档 三·4：配置的「定义 + 解析」只有一处）。
//!
//! 各模块（db / cache / queue / auth / log…）不再各自解析配置，只引用本目录
//! 的结构体。业务配置节由应用在自己侧扩展（`#[serde(flatten)]` 框架根）。

mod auth; // 认证相关配置节模块
mod authz; // 授权（Casbin）配置节模块
mod cache; // 缓存配置节模块
mod database; // 数据库配置节模块
mod i18n; // 国际化配置节模块
mod log; // 日志配置节模块
mod queue; // 消息队列配置节模块
mod realtime; // 实时通信配置节模块
mod redact; // 连接串脱敏工具（各节手写 Debug 共用）
mod resilience; // 弹性（熔断/重试/降级）配置节模块
mod server; // HTTP 服务配置节模块
mod task; // 定时任务配置节模块
mod time; // 展示时区配置节模块（可选）

pub use auth::{AuthSettings, JwtSettings, OAuth2Provider, OAuth2Settings, PasswordPolicy, SessionSettings}; // 导出认证相关配置类型
pub use authz::AuthzSettings; // 导出授权配置类型
pub use cache::{CacheMemorySettings, CacheRedisSettings, CacheSettings}; // 导出缓存相关配置类型
pub use database::DatabaseSettings; // 导出数据库配置类型
pub use i18n::I18nSettings; // 导出国际化配置类型
pub use log::{FileLogSettings, LogSettings}; // 导出日志相关配置类型
pub use queue::{ // 导出消息队列相关配置类型
    QueueKafkaSettings, QueueMemorySettings, QueueNatsSettings, QueueRabbitmqSettings, // 各队列后端配置类型
    QueueRedisSettings, QueueSettings, // Redis 队列配置与队列总配置
};
pub use realtime::RealtimeSettings; // 导出实时通信配置类型
pub use resilience::{ResiliencePolicy, ResilienceSettings}; // 导出弹性策略与弹性配置类型
pub use server::{ // 导出 HTTP 服务相关配置类型
    CorsSettings, CsrfSettings, FileBodySettings, IdempotencySettings, IpFilterSettings, // 跨域/CSRF/文件体/幂等/IP 过滤配置
    MetricsSettings, RateLimitSettings, SecurityHeadersSettings, ServerSettings, TlsSettings, WatchSettings, // 指标/限流/安全头/服务/TLS/热更新配置
};
pub use task::{JobSettings, TaskSettings}; // 导出定时任务相关配置类型
pub use time::TimeSettings; // 导出展示时区配置类型

pub(crate) use redact::redact_url; // 供各配置节手写 Debug 时脱敏连接串
