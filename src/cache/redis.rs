//! Redis 缓存后端（feature = "cache-redis"）：deadpool-redis 连接池。
//! 多实例部署使用；分布式锁（`SET NX PX`）实现同样在本文件。

use std::time::Duration;

use deadpool_redis::{Config as PoolConfig, Runtime};

use super::lock::{Lock, LockError, LockGuard};
use super::{Cache, CacheError};
use crate::config::sections::CacheRedisSettings;

/// 共享 Redis 连接池（cache 后端与 queue-redis 后端可分别建池；同 url 亦互不影响）
#[derive(Debug, Clone)]
pub struct RedisPool {
    pub pool: deadpool_redis::Pool,
}

impl RedisPool {
    pub fn from_cache_settings(settings: &CacheRedisSettings) -> Result<Self, CacheError> {
        Self::from_url(&settings.url, settings.pool_size).map_err(Into::into)
    }

    pub fn from_url(url: &str, pool_size: u32) -> Result<Self, deadpool_redis::CreatePoolError> {
        let mut pool_cfg = PoolConfig::from_url(url.to_string());
        if pool_size > 0 {
            pool_cfg.pool = Some(deadpool_redis::PoolConfig {
                max_size: pool_size as usize,
                ..Default::default()
            });
        }
        Ok(Self {
            pool: pool_cfg.create_pool(Some(Runtime::Tokio1))?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RedisCache {
    pool: deadpool_redis::Pool,
}

impl RedisCache {
    pub fn new(settings: &CacheRedisSettings) -> Result<Self, CacheError> {
        Ok(Self {
            pool: RedisPool::from_cache_settings(settings)?.pool,
        })
    }

    pub(crate) async fn conn(
        &self,
    ) -> Result<deadpool_redis::Connection, deadpool_redis::PoolError> {
        self.pool.get().await
    }
}

#[async_trait::async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let mut conn = self.conn().await?;
        Ok(deadpool_redis::redis::cmd("GET")
            .arg(key)
            .query_async::<Option<String>>(&mut conn)
            .await?)
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        let mut conn = self.conn().await?;
        let mut cmd = deadpool_redis::redis::cmd("SET");
        cmd.arg(key).arg(value);
        if let Some(d) = ttl {
            cmd.arg("EX").arg(d.as_secs().max(1));
        }
        cmd.query_async::<()>(&mut conn).await?;
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<(), CacheError> {
        let mut conn = self.conn().await?;
        deadpool_redis::redis::cmd("DEL")
            .arg(key)
            .query_async::<()>(&mut conn)
            .await?;
        Ok(())
    }

    /// 原生 `INCRBY`：多实例共享同一计数（限流 / 幂等计数的推荐后端）
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError> {
        let mut conn = self.conn().await?;
        Ok(deadpool_redis::redis::cmd("INCRBY")
            .arg(key)
            .arg(delta)
            .query_async::<i64>(&mut conn)
            .await?)
    }

    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        let mut conn = self.conn().await?;
        let touched: i64 = match ttl {
            Some(d) if d > Duration::ZERO => {
                deadpool_redis::redis::cmd("EXPIRE")
                    .arg(key)
                    .arg(d.as_secs().max(1))
                    .query_async(&mut conn)
                    .await?
            }
            _ => {
                deadpool_redis::redis::cmd("PERSIST")
                    .arg(key)
                    .query_async(&mut conn)
                    .await?
            }
        };
        Ok(touched == 1)
    }

    async fn ping(&self) -> Result<(), CacheError> {
        let mut conn = self.conn().await?;
        deadpool_redis::redis::cmd("PING")
            .query_async::<()>(&mut conn)
            .await?;
        Ok(())
    }
}

// ---------- 分布式锁（SET NX PX + Lua 持有者校验） ----------

const RELEASE_LUA: &str =
    "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end";

const EXTEND_LUA: &str =
    "if redis.call('get', KEYS[1]) == ARGV[1] then redis.call('set', KEYS[1], ARGV[1], 'PX', ARGV[2]) return 1 else return 0 end";

#[derive(Debug, Clone)]
pub struct RedisLock {
    pool: deadpool_redis::Pool,
    key_prefix: String,
}

impl RedisLock {
    pub fn new(settings: &CacheRedisSettings) -> Result<Self, CacheError> {
        Ok(Self {
            pool: RedisPool::from_cache_settings(settings)?.pool,
            key_prefix: if settings.lock_prefix.is_empty() {
                "core-rs:lock:".to_string()
            } else {
                settings.lock_prefix.clone()
            },
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.key_prefix, key)
    }
}

#[async_trait::async_trait]
impl Lock for RedisLock {
    async fn try_acquire(self: std::sync::Arc<Self>, key: &str, ttl: Duration) -> Result<Option<LockGuard>, LockError> {
        // ttl 为零（含亚毫秒）语义 = 不锁；EX 是秒粒度，亚秒 TTL 会被放大成 1 秒
        if ttl < Duration::from_millis(1) {
            return Ok(None);
        }
        let token = uuid::Uuid::new_v4().to_string();
        let full = self.full_key(key);
        let mut conn = self
            .pool
            .get()
            .await
            .map_err(|e| LockError::Backend(e.to_string()))?;
        let ok: Option<String> = deadpool_redis::redis::cmd("SET")
            .arg(&full)
            .arg(&token)
            .arg("NX")
            // PX（毫秒）：EX 会把 200ms 放大成 1s，锁的互斥窗口与调用方预期不符
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async(&mut conn)
            .await
            .map_err(|e| LockError::Backend(e.to_string()))?;
        Ok(ok.map(|_| LockGuard {
            backend: self,
            // guard 存裸 key：release/extend 内部统一过 full_key，两后端契约一致
            key: key.to_string(),
            token,
            released: false,
        }))
    }

    async fn release(&self, key: &str, token: &str) -> Result<(), LockError> {
        let mut conn = self
            .pool
            .get()
            .await
            .map_err(|e| LockError::Backend(e.to_string()))?;
        // key 与 try_acquire 同一契约：业务裸 key，这里补前缀
        deadpool_redis::redis::Script::new(RELEASE_LUA)
            .key(self.full_key(key))
            .arg(token)
            .invoke_async::<()>(&mut conn)
            .await
            .map_err(|e| LockError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn extend(&self, key: &str, token: &str, ttl: Duration) -> Result<bool, LockError> {
        if ttl < Duration::from_millis(1) {
            return Ok(false);
        }
        let mut conn = self
            .pool
            .get()
            .await
            .map_err(|e| LockError::Backend(e.to_string()))?;
        let renewed: i64 = deadpool_redis::redis::Script::new(EXTEND_LUA)
            .key(self.full_key(key))
            .arg(token)
            .arg(ttl.as_millis().max(1) as u64)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| LockError::Backend(e.to_string()))?;
        Ok(renewed == 1)
    }
}

