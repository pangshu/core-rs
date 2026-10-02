//! Redis 连接池初始化（deadpool-redis）。

use deadpool_redis::{Config as PoolConfig, Runtime};

use crate::cache::{Backend, Cache, CacheError};
use crate::config::RedisConfig;

/// 按配置建立 redis 连接池并包装为 redis 后端的 [`Cache`]。
/// url 为空时由调用方（app 构建器）跳过。
pub fn connect(cfg: &RedisConfig) -> Result<Cache, CacheError> {
    Ok(Cache {
        backend: Backend::Redis(connect_pool(cfg)?),
        inflight: Default::default(),
    })
}

/// 仅建立裸连接池（cache::build 的 redis / auto 后端使用）
pub(crate) fn connect_pool(cfg: &RedisConfig) -> Result<deadpool_redis::Pool, CacheError> {
    let mut pool_cfg = PoolConfig::from_url(cfg.url.clone());
    if cfg.pool_size > 0 {
        pool_cfg.pool = Some(deadpool_redis::PoolConfig {
            max_size: cfg.pool_size as usize,
            ..Default::default()
        });
    }
    Ok(pool_cfg.create_pool(Some(Runtime::Tokio1))?)
}
