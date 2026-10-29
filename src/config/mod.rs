//! 配置：**框架管机制，应用管布局**。
//!
//! - [`ConfigSource`]：唯一的加载入口。**读哪些文件、顺序、环境变量前缀、运行环境**
//!   全部由应用层显式给出；框架不认识任何具体路径。
//! - [`Environment`]：环境是框架类型（生产护栏的分支条件），但取值由应用层传入。
//! - [`preset`]：**可选**的 4 环境约定（`{env}.toml`），想用就用。
//! - [`AppConfig`]：强类型配置 + [`AppConfig::validate`] 的全部 fail-fast 校验。
//! - [`ConfigHandle`]：可廉价共享的快照，热更新原子替换。
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use aurora::config::{ConfigSource, Environment};
//!
//! let config = ConfigSource::new(Environment::Development)
//!     .file("config/base.toml")
//!     .file("config/development.toml")
//!     .load()?;
//! # let _ = config;
//! # Ok(())
//! # }
//! ```
//!
//! 字段定义见 `docs/08-config-spec.md`。

mod app_config;
pub(crate) mod environment;
mod handle;
mod loader;
#[cfg(feature = "nacos")]
mod nacos;
pub mod preset;
pub mod sections;
#[cfg(feature = "watch")]
mod watch;

pub use environment::Environment;
pub use loader::ConfigSource;
#[cfg(feature = "nacos")]
pub use nacos::{NacosClient, NacosWatcher};
#[cfg(feature = "auth")]
pub use sections::AuthSection;
#[cfg(feature = "csrf")]
pub use sections::CsrfSection;
#[cfg(feature = "db")]
pub use sections::DatabaseSection;
#[cfg(feature = "geoip")]
pub use sections::GeoIpSection;
#[cfg(feature = "i18n")]
pub use sections::I18nSection;
#[cfg(feature = "idempotency")]
pub use sections::IdempotencySection;
#[cfg(feature = "openapi")]
pub use sections::OpenApiSection;
#[cfg(any(feature = "queue", feature = "resilience"))]
pub use sections::RetryBackoff;
#[cfg(feature = "task")]
pub use sections::SchedulerSection;
#[cfg(feature = "search")]
pub use sections::SearchSection;
#[cfg(feature = "watch")]
pub use sections::WatchSection;
pub use sections::{
    AppSection, CorsSection, DefaultOffToggle, FileOverflow, FileRotation, FileSection,
    FileWriteMode, HttpSection, MetricsSection, OtelSection, ServerSection, TelemetrySection,
    ToggleSection,
};
#[cfg(feature = "authz")]
pub use sections::{AuthzAdapter, AuthzSection};
#[cfg(feature = "resilience")]
pub use sections::{
    BulkheadSection, CircuitBreakerSection, ResilienceRetrySection, ResilienceSection,
    TimeoutSection,
};
#[cfg(feature = "cache")]
pub use sections::{CacheBackend, CacheSection, LockSection, RedisSection, SingleflightSection};
#[cfg(any(feature = "ws", feature = "sse"))]
pub use sections::{InboundOverflow, RealtimeSection};
#[cfg(feature = "ip-filter")]
pub use sections::{IpFilterMode, IpFilterSection};
#[cfg(feature = "nacos")]
pub use sections::{NacosFormat, NacosSection};
#[cfg(feature = "tls")]
pub use sections::{OcspSection, TlsSection};
#[cfg(feature = "queue")]
pub use sections::{QueueBackendKind, QueueRetrySection, QueueSection};
#[cfg(feature = "ratelimit")]
pub use sections::{RateLimitRule, RateLimitSection, RateLimitStore, RateLimitStrategy};
#[cfg(feature = "security")]
pub use sections::{SecuritySection, ValidationSection};
#[cfg(feature = "watch")]
pub use watch::ConfigWatcher;

pub use app_config::AppConfig;
pub use handle::{ConfigHandle, SecurityBoundary};
