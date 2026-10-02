use serde::{Deserialize, Serialize};

fn default_cache_type() -> String {
    "auto".to_string()
}
fn default_max_capacity() -> u64 {
    10_000
}

/// `[cache]` 配置段：缓存后端选择。
///
/// - `redis`：使用 `[redis].url`（需默认依赖 deadpool-redis）
/// - `memory`：进程内 TTL 缓存（feature = "cache-memory"）
/// - `auto`（默认）：`[redis].url` 非空走 redis，否则走 memory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// auto | redis | memory
    #[serde(rename = "type", default = "default_cache_type")]
    pub backend: String,
    /// memory 后端参数（feature = "cache-memory"）
    #[serde(default)]
    pub memory: MemoryConfig,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            backend: default_cache_type(),
            memory: MemoryConfig::default(),
        }
    }
}

/// `[cache.memory]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// 条目数上限（超过后按 LRU + TTL 淘汰）
    #[serde(default = "default_max_capacity")]
    pub max_capacity: u64,
    /// 条目未显式传 ttl 时的默认过期时间（秒）；0 表示不过期
    #[serde(default)]
    pub default_ttl_secs: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_capacity: default_max_capacity(),
            default_ttl_secs: 0,
        }
    }
}
