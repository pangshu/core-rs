//! `[cache]` 配置节：backend(memory/redis) + 各后端参数（文档 三·11）。
//!
//! - `memory`（默认）：moka 进程内 TTL 缓存 + 进程内锁，零外部依赖；
//! - `redis`：deadpool-redis 连接池 + SET NX PX 分布式锁，多实例部署使用。

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

fn default_backend() -> String { // 默认后端名取值函数
    "memory".to_string() // 默认使用进程内 memory 后端
}
fn default_max_capacity() -> u64 { // 默认容量取值函数
    10_000 // 默认 10000 条
}

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct CacheSettings { // 定义 `[cache]` 配置结构体
    /// memory | redis
    #[serde(default = "default_backend")] // 缺省为 memory
    pub backend: String, // 缓存后端选择
    /// memory 后端参数
    #[serde(default)] // 缺省用 memory 后端默认值
    pub memory: CacheMemorySettings, // memory 后端参数
    /// redis 后端参数
    #[serde(default)] // 缺省用 redis 后端默认值
    pub redis: CacheRedisSettings, // redis 后端参数
}

impl Default for CacheSettings { // 为 CacheSettings 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            backend: default_backend(), // 默认 memory
            memory: CacheMemorySettings::default(), // memory 默认参数
            redis: CacheRedisSettings::default(), // redis 默认参数
        }
    }
}

/// `[cache.memory]`
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct CacheMemorySettings { // 定义 `[cache.memory]` 配置
    /// 条目数上限（超过后按 LRU + TTL 淘汰）
    #[serde(default = "default_max_capacity")] // 缺省为 10000
    pub max_capacity: u64, // 条目数上限
    /// 条目未显式传 ttl 时的默认过期时间（秒）；0 表示不过期
    #[serde(default)] // 缺省为 0（不过期）
    pub default_ttl_secs: u64, // 默认过期时间（秒）
}

impl Default for CacheMemorySettings { // 为 memory 配置手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            max_capacity: default_max_capacity(), // 默认 10000
            default_ttl_secs: 0, // 默认不过期
        }
    }
}

/// `[cache.redis]`
#[derive(Clone, Serialize, Deserialize, Default)] // 派生克隆/serde/默认值（Debug 手写）
pub struct CacheRedisSettings { // 定义 `[cache.redis]` 配置
    /// Redis 连接串；空串视为未启用
    #[serde(default)] // 缺省为空串（未启用）
    pub url: String, // Redis 连接串
    /// 连接池大小，0 表示用 deadpool 默认值
    #[serde(default)] // 缺省为 0（用 deadpool 默认）
    pub pool_size: u32, // 连接池大小
    /// 锁 key 前缀（多应用共享一个 Redis 时隔离键空间）
    #[serde(default = "default_lock_prefix")] // 缺省为 core-rs:lock:
    pub lock_prefix: String, // 锁 key 前缀
}

fn default_lock_prefix() -> String { // 锁前缀默认值函数
    "core-rs:lock:".to_string() // 默认前缀
}

/// 手写 Debug：连接串脱敏
impl std::fmt::Debug for CacheRedisSettings { // 手写 Debug，连接串脱敏
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("CacheRedisSettings") // 开始构造调试输出
            .field("url", &super::redact_url(&self.url)) // url 字段脱敏输出
            .field("pool_size", &self.pool_size) // 输出连接池大小
            .field("lock_prefix", &self.lock_prefix) // 输出锁前缀
            .finish() // 结束并生成调试输出
    }
}

impl CacheRedisSettings { // 为 redis 配置实现方法
    pub fn enabled(&self) -> bool { // 判断 redis 后端是否已配置
        !self.url.is_empty() // url 非空即启用
    }
}
