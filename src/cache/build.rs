//! 按 `[cache]` 配置构建缓存实例（App 装配时自动调用；手动装配亦可用）。

use crate::config::sections::CacheSettings; // 引入 [cache] 配置节，构建缓存/锁时读取

use super::{CacheError, CacheHandle}; // 引入缓存错误类型与共享句柄

/// 按 `[cache]` 配置构建缓存实例（App 装配时自动调用；手动装配亦可用）。
pub fn build_cache(settings: &CacheSettings) -> Result<CacheHandle, CacheError> { // 依据配置选择并构造缓存后端
    match settings.backend.as_str() { // 按 backend 字段分派
        "memory" => { // 内存后端分支
            #[cfg(feature = "cache-memory")] // 开启内存 feature 时使用下面实现
            {
                Ok(std::sync::Arc::new(super::memory::MemoryCache::new(&settings.memory))) // 用内存配置构造并包成句柄
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
                Ok(std::sync::Arc::new(super::redis::RedisCache::new(&settings.redis)?)) // 用 Redis 配置构造并包成句柄
            }
            #[cfg(not(feature = "cache-redis"))] // 未开启 Redis feature 时使用下面实现
            {
                Err(CacheError::FeatureDisabled("redis".to_string())) // 返回 feature 未启用错误
            }
        }
        other => Err(CacheError::UnknownBackend(other.to_string())), // 其余值视为未知后端
    }
}
