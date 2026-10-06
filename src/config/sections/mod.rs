//! 各子系统配置结构，集中定义于此（文档 三·4：配置的「定义 + 解析」只有一处）。
//!
//! 各模块（db / cache / queue / auth / log…）不再各自解析配置，只引用本目录
//! 的结构体。业务配置节由应用在自己侧扩展（`#[serde(flatten)]` 框架根）。

mod auth;
mod authz;
mod cache;
mod database;
mod i18n;
mod log;
mod queue;
mod realtime;
mod resilience;
mod server;
mod task;

pub use auth::{AuthSettings, JwtSettings, OAuth2Provider, OAuth2Settings, PasswordPolicy, SessionSettings};
pub use authz::AuthzSettings;
pub use cache::{CacheMemorySettings, CacheRedisSettings, CacheSettings};
pub use database::DatabaseSettings;
pub use i18n::I18nSettings;
pub use log::{FileLogSettings, LogSettings};
pub use queue::{
    QueueKafkaSettings, QueueMemorySettings, QueueNatsSettings, QueueRabbitmqSettings,
    QueueRedisSettings, QueueSettings,
};
pub use realtime::RealtimeSettings;
pub use resilience::{ResiliencePolicy, ResilienceSettings};
pub use server::{
    CorsSettings, CsrfSettings, FileBodySettings, IdempotencySettings, IpFilterSettings,
    MetricsSettings, RateLimitSettings, SecurityHeadersSettings, ServerSettings, WatchSettings,
};
pub use task::{JobSettings, TaskSettings};

/// 连接串脱敏（各配置节手写 `Debug` 用）：`scheme://user:pass@host/db` →
/// `scheme://***@host/db`。Settings 及各节大量派生 `#[derive(Debug)]`，
/// 任何一处 `{:?}` 都会把 DB 密码 / JWT secret 打进日志——含敏感字段的节
/// 一律手写 Debug 并经过这里。
pub(crate) fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None if url.is_empty() => String::new(),
        None => "***".to_string(),
    }
}
