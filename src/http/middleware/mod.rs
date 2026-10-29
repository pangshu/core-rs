//! HTTP 中间件。
//!
//! 装配顺序由 `stack` 统一保证（由外到内）：
//! `panic → request_id → trace → access_log → metrics → security_headers →
//! envelope(413) → body_limit → envelope(408) → timeout → ip_filter → cors →
//! compression → locale → csrf → rate_limit → auth → idempotency`。
//!
//! 全部中间件均已落地：M1 的 `panic` / `request_id` / `trace` / `access_log` /
//! `security_headers` / `body_limit` / `timeout` / `cors` / `compression` /
//! `envelope`，M5 的 `ratelimit` / `csrf` / `idempotency` / `ip_filter` / `locale`。

pub(crate) mod body_limit;
pub(crate) mod compression;
pub(crate) mod cors;
pub(crate) mod envelope;
pub(crate) mod panic;
pub(crate) mod request_id;
pub(crate) mod stack;
pub(crate) mod timeout;
pub(crate) mod trace;

#[cfg(feature = "access-log")]
pub(crate) mod access_log;
#[cfg(feature = "auth")]
pub(crate) mod auth_guard;
#[cfg(feature = "csrf")]
pub(crate) mod csrf;
#[cfg(feature = "idempotency")]
pub(crate) mod idempotency;
#[cfg(feature = "ip-filter")]
pub(crate) mod ip_filter;
#[cfg(feature = "i18n")]
pub(crate) mod locale;
#[cfg(feature = "ratelimit")]
pub(crate) mod rate_limit;
#[cfg(feature = "security-headers")]
pub(crate) mod security_headers;
