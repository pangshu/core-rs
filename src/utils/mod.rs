//! 通用工具模块。
//!
//! 提供时间 / 时区、ID 生成、客户端 IP 提取、分页模型与字符串工具。
//! ⚠️ 模块名是 `utils`（复数）。

pub(crate) mod amqp_url;
pub mod client_ip;
pub mod ct;
pub mod id;
pub mod pagination;
#[cfg(any(
    feature = "cache-redis",
    feature = "queue-redis",
    feature = "ratelimit-redis",
    feature = "session-redis"
))]
pub(crate) mod redis;
pub(crate) mod redis_url;
pub mod secret;
#[cfg(feature = "snowflake")]
pub mod snowflake;
pub mod string;
pub mod time;

pub use client_ip::{ClientIp, ProxyTrust};
pub use ct::constant_time_eq;
pub use id::Id;
pub use pagination::{CursorPage, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, Page};
pub use secret::Secret;
pub use time::{now_unix_secs, now_utc, parse_timezone};
