//! `[cache]` 配置节：backend(memory/redis) + 各后端参数（文档 三·11）。
//!
//! - `memory`（默认）：moka 进程内 TTL 缓存 + 进程内锁，零外部依赖；
//! - `redis`：deadpool-redis 连接池 + SET NX PX 分布式锁，多实例部署使用。

use serde::{Deserialize, Serialize};

fn default_backend() -> String {
    "memory".to_string()
}
fn default_max_capacity() -> u64 {
    10_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSettings {
    /// memory | redis
    #[serde(default = "default_backend")]
    pub backend: String,
    /// memory 后端参数
    #[serde(default)]
    pub memory: CacheMemorySettings,
    /// redis 后端参数
    #[serde(default)]
    pub redis: CacheRedisSettings,
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            memory: CacheMemorySettings::default(),
            redis: CacheRedisSettings::default(),
        }
    }
}

/// `[cache.memory]`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMemorySettings {
    /// 条目数上限（超过后按 LRU + TTL 淘汰）
    #[serde(default = "default_max_capacity")]
    pub max_capacity: u64,
    /// 条目未显式传 ttl 时的默认过期时间（秒）；0 表示不过期
    #[serde(default)]
    pub default_ttl_secs: u64,
}

impl Default for CacheMemorySettings {
    fn default() -> Self {
        Self {
            max_capacity: default_max_capacity(),
            default_ttl_secs: 0,
        }
    }
}

/// `[cache.redis]`
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct CacheRedisSettings {
    /// Redis 连接串；空串视为未启用
    #[serde(default)]
    pub url: String,
    /// 连接池大小，0 表示用 deadpool 默认值
    #[serde(default)]
    pub pool_size: u32,
    /// 锁 key 前缀（多应用共享一个 Redis 时隔离键空间）
    #[serde(default = "default_lock_prefix")]
    pub lock_prefix: String,
}

fn default_lock_prefix() -> String {
    "core-rs:lock:".to_string()
}

/// 手写 Debug：连接串脱敏
impl std::fmt::Debug for CacheRedisSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheRedisSettings")
            .field("url", &super::redact_url(&self.url))
            .field("pool_size", &self.pool_size)
            .field("lock_prefix", &self.lock_prefix)
            .finish()
    }
}

impl CacheRedisSettings {
    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }
}
