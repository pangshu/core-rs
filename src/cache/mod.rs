//! 缓存与锁（可插拔：内存 / Redis，配置选择，文档 三·11）。
//!
//! - [`Cache`] / [`Lock`] trait 是唯一契约，业务只依赖 trait，**换后端不改一行业务代码**；
//! - [`memory`]：moka 进程内缓存 + 进程内锁（feature = "cache-memory"，默认），
//!   单机部署与开发/测试的默认值；
//! - [`redis`]：deadpool-redis 连接池 + `SET NX PX` 分布式锁（feature = "cache-redis"），
//!   多实例部署使用；限流 / 幂等 / 分布式锁等需要**跨进程一致**的场景必须 redis；
//! - [`lock`]：`Lock` trait + [`LockGuard`]（释放 Lua 校验持有者 / Drop 兜底），
//!   是限流、防重、幂等的公共底座。

#[cfg(feature = "cache-memory")] // 仅在开启内存缓存 feature 时编译下面模块
pub mod memory; // 进程内 moka 缓存后端（默认）
#[cfg(feature = "cache-redis")] // 仅在开启 Redis 缓存 feature 时编译下面模块
pub mod redis; // Redis 缓存后端与分布式锁实现
pub mod lock; // 锁 trait 与进程内锁实现（两种后端都依赖）

pub use lock::{Lock, LockError, LockGuard, LockHandle, MemoryLock, build_lock}; // 对外重导出锁相关公共类型与构建函数

use std::time::Duration; // 引入时长类型，用于 TTL 参数

use serde::de::DeserializeOwned; // 引入反序列化 trait，get_json 需要
use serde::Serialize; // 引入序列化 trait，set_json 需要

use crate::config::sections::CacheSettings; // 引入 [cache] 配置节，构建缓存/锁时读取

/// 缓存操作错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 与 thiserror 错误实现
pub enum CacheError { // 缓存统一错误枚举
    #[error("create redis pool failed: {0}")] // 错误文案：创建 Redis 连接池失败
    #[cfg(feature = "cache-redis")] // 仅在 Redis feature 下存在该变体
    Create(#[from] deadpool_redis::CreatePoolError), // 包装连接池创建错误并自动转换
    #[error("redis pool error: {0}")] // 错误文案：Redis 连接池取连接失败
    #[cfg(feature = "cache-redis")] // 仅在 Redis feature 下存在该变体
    Pool(#[from] deadpool_redis::PoolError), // 包装连接池运行时错误
    #[error("redis error: {0}")] // 错误文案：Redis 命令执行错误
    #[cfg(feature = "cache-redis")] // 仅在 Redis feature 下存在该变体
    Redis(#[from] deadpool_redis::redis::RedisError), // 包装底层 Redis 错误
    #[error("cache serialization error: {0}")] // 错误文案：JSON 序列化/反序列化失败
    Serde(#[from] serde_json::Error), // 包装 serde_json 错误
    #[error("cache backend `{0}` requires a feature (cache-memory / cache-redis) that is not enabled")] // 错误文案：后端所需 feature 未开启
    FeatureDisabled(String), // 记录所需后端名，提示未编译对应 feature
    #[error("cache backend `{0}` is misconfigured: {1}")] // 错误文案：后端配置有误
    Config(String, String), // 记录后端名与具体配置问题
    #[error("unknown cache backend: {0} (expected memory / redis)")] // 错误文案：未知后端名
    UnknownBackend(String), // 记录无法识别的后端名
}

/// 缓存契约（string 级原语，保持对象安全；JSON / 回源便捷方法见 [`CacheExt`]）。
#[async_trait::async_trait] // 启用 async_trait，使 trait 可含 async 方法且对象安全
pub trait Cache: Send + Sync { // 缓存后端统一契约，要求线程安全
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>; // 按键读取字符串值，未命中为 None
    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError>; // 写入键值并可选设置 TTL
    async fn del(&self, key: &str) -> Result<(), CacheError>; // 删除指定键
    /// 原子计数：key 不存在时从 0 起算（等价 redis `INCRBY`），新建计数器无 TTL。
    /// memory 后端进程内正确，进程间无共享——限流/分布式计数请配 redis 后端。
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError>; // 对计数键原子累加并返回新值
    /// 重设 key 的 TTL：`None`/`Duration::ZERO` 表示永不过期（等价 `PERSIST`）；
    /// key 不存在返回 `Ok(false)`。
    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError>; // 重设 TTL，返回是否实际命中键
    /// 存活探测（/ready 用）。memory 后端恒 Ok。
    async fn ping(&self) -> Result<(), CacheError>; // 探活后端，供就绪检查聚合
}

/// 便捷方法（JSON 读写 / cache-aside 一站式回源），对 `dyn Cache` 同样可用。
#[allow(async_fn_in_trait)] // 抑制 async fn in trait 的 lint（本 trait 不做对象安全要求）
pub trait CacheExt: Cache { // 在 Cache 之上扩展 JSON 与回源便捷方法
    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, CacheError> { // 读取并反序列化为 T，未命中为 None
        let Some(s) = self.get(key).await? else { // 先取原始字符串，未命中则走 else 分支
            return Ok(None); // 缓存未命中直接返回 None
        };
        match serde_json::from_str(&s) { // 尝试把字符串反序列化为目标类型
            Ok(v) => Ok(Some(v)), // 反序列化成功，返回 Some
            // 脏数据（如缓存结构变更）按 miss 处理让业务回源，而不是打挂接口
            Err(e) => { // 反序列化失败时的降级分支
                tracing::warn!(key = %key, error = %e, "cache value corrupt, treat as miss"); // 记录告警日志
                Ok(None) // 按未命中返回，交由业务回源
            }
        }
    }

    async fn set_json<T: Serialize + Sync>( // 把值序列化为 JSON 后写入缓存
        &self, // 方法接收者：缓存实例引用
        key: &str, // 缓存键
        value: &T, // 待序列化的值
        ttl: Option<Duration>, // 可选 TTL，None 表示不过期
    ) -> Result<(), CacheError> { // 返回单元结果
        self.set(key, &serde_json::to_string(value)?, ttl).await // 序列化并写入，返回写入结果
    }

    /// cache-aside 一站式读取：先查缓存，miss（**或缓存故障**）时用 `load` 回源并回填。
    /// 缓存读写失败只记日志、按 miss 处理，**不会传染成业务 500**；
    /// `load` 的错误类型 `E` 原样透传（通常是 `AppError`）。
    async fn get_or_load<T, E, F, Fut>(&self, key: &str, ttl: Option<Duration>, load: F) -> Result<T, E> // 先查缓存，miss 则调用 load 回源并回填
    where // 泛型约束子句开始
        T: Serialize + DeserializeOwned + Send + Sync, // 缓存值需可序列化/反序列化且线程安全
        E: From<CacheError>, // 业务错误需能由缓存错误转换而来
        F: FnOnce() -> Fut + Send, // 回源闭包只调用一次且可跨线程
        Fut: std::future::Future<Output = Result<T, E>> + Send, // 回源返回的未来类型约束
    {
        // 读缓存：任何缓存故障按 miss 降级，不传染
        match self.get_json::<T>(key).await { // 尝试从缓存读取
            Ok(Some(v)) => return Ok(v), // 命中直接返回
            Ok(None) => {} // 未命中则继续往下回源
            Err(e) => { // 缓存读故障时降级
                tracing::warn!(key = %key, error = %e, "cache read failed, degrade to direct load") // 记录告警但不中断
            }
        }
        let value = load().await?; // 调用回源闭包取真实数据，错误原样透传
        if let Err(e) = self.set_json(key, &value, ttl).await { // 尝试回填缓存
            tracing::warn!(key = %key, error = %e, "cache write failed after load"); // 回填失败仅记日志，不影响返回
        }
        Ok(value) // 返回回源得到的值
    }
}

impl<T: Cache + ?Sized> CacheExt for T {} // 为所有 Cache 实现（含 dyn）自动实现 CacheExt

/// 共享缓存句柄（存于 CoreState）
pub type CacheHandle = std::sync::Arc<dyn Cache>; // 类型别名：线程安全的缓存 trait 对象句柄

/// 按 `[cache]` 配置构建缓存实例（App 装配时自动调用；手动装配亦可用）。
pub fn build_cache(settings: &CacheSettings) -> Result<CacheHandle, CacheError> { // 依据配置选择并构造缓存后端
    match settings.backend.as_str() { // 按 backend 字段分派
        "memory" => { // 内存后端分支
            #[cfg(feature = "cache-memory")] // 开启内存 feature 时使用下面实现
            {
                Ok(std::sync::Arc::new(memory::MemoryCache::new(&settings.memory))) // 用内存配置构造并包成句柄
            }
            #[cfg(not(feature = "cache-memory"))] // 未开启内存 feature 时使用下面实现
            {
                Err(CacheError::FeatureDisabled("memory".to_string())) // 返回 feature 未启用错误
            }
        }
        "redis" => { // Redis 后端分支
            #[cfg(feature = "cache-redis")] // 开启 Redis feature 时使用下面实现
            {
                if !settings.redis.enabled() { // 校验 redis.url 是否已配置
                    return Err(CacheError::Config( // 未配置则返回配置错误
                        "redis".to_string(), // 出错的后端名
                        "cache.backend = redis but cache.redis.url is empty".to_string(), // 具体配置问题说明
                    ));
                }
                Ok(std::sync::Arc::new(redis::RedisCache::new(&settings.redis)?)) // 用 Redis 配置构造并包成句柄
            }
            #[cfg(not(feature = "cache-redis"))] // 未开启 Redis feature 时使用下面实现
            {
                Err(CacheError::FeatureDisabled("redis".to_string())) // 返回 feature 未启用错误
            }
        }
        other => Err(CacheError::UnknownBackend(other.to_string())), // 其余值视为未知后端
    }
}
