//! 极简分布式锁（feature = "dist-lock"）：redis `SET NX EX` + token 校验释放（Lua）。
//!
//! 仅 redis 后端可用：`cache.type = memory` 时 [`Cache::try_lock`] 返回
//! [`CacheError::RequiresRedis`]。
//!
//! 未实现自动看门狗续期：长任务请自行调用 [`DistLock::extend`]，或把 ttl 设为
//! 任务时长的足够余量。Drop 时 best-effort 释放（进程存活时由后台任务执行）。

use std::time::Duration;

use crate::cache::{Cache, CacheError};

const RELEASE_LUA: &str =
    "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end";

const EXTEND_LUA: &str =
    "if redis.call('get', KEYS[1]) == ARGV[1] then redis.call('set', KEYS[1], ARGV[1], 'EX', ARGV[2]) return 1 else return 0 end";

#[derive(Debug)]
pub struct DistLock {
    pool: deadpool_redis::Pool,
    key: String,
    token: String,
}

impl Cache {
    /// 尝试获取分布式锁；`None` 表示锁已被其他实例持有。
    pub async fn try_lock(
        &self,
        key: &str,
        ttl: Duration,
    ) -> Result<Option<DistLock>, CacheError> {
        let pool = self.redis_pool().ok_or(CacheError::RequiresRedis)?;
        let token = uuid::Uuid::new_v4().to_string();
        let mut conn = pool.get().await?;
        let ok: Option<String> = deadpool_redis::redis::cmd("SET")
            .arg(key)
            .arg(&token)
            .arg("NX")
            .arg("EX")
            .arg(ttl.as_secs().max(1))
            .query_async(&mut conn)
            .await?;
        Ok(ok.map(|_| DistLock {
            pool: pool.clone(),
            key: key.to_string(),
            token,
        }))
    }
}

impl DistLock {
    /// 释放锁（token 校验，不会误删他人的锁）。已过期/被抢占时静默成功。
    pub async fn release(self) -> Result<(), CacheError> {
        match self.release_inner().await {
            // 释放成功后跳过 Drop，避免再 spawn 一次注定空跑的释放任务
            Ok(()) => {
                std::mem::forget(self);
                Ok(())
            }
            // 释放失败则照常 Drop，由 Drop 里的 best-effort 重试兜底
            Err(e) => Err(e),
        }
    }

    async fn release_inner(&self) -> Result<(), CacheError> {
        let mut conn = self.pool.get().await?;
        deadpool_redis::redis::Script::new(RELEASE_LUA)
            .key(&self.key)
            .arg(&self.token)
            .invoke_async::<()>(&mut conn)
            .await?;
        Ok(())
    }

    /// 续期（Lua 比对 token，仅当前持锁者可续）。
    /// 返回 `false` 表示已失去锁（过期 / 被抢占），调用方应停止受锁保护的工作。
    pub async fn extend(&self, ttl: Duration) -> Result<bool, CacheError> {
        let mut conn = self.pool.get().await?;
        let renewed: i64 = deadpool_redis::redis::Script::new(EXTEND_LUA)
            .key(&self.key)
            .arg(&self.token)
            .arg(ttl.as_secs().max(1))
            .invoke_async(&mut conn)
            .await?;
        Ok(renewed == 1)
    }
}

impl Drop for DistLock {
    fn drop(&mut self) {
        // best-effort：不在 runtime 上下文中时跳过，靠 TTL 过期兜底
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let lock = DistLock {
                pool: self.pool.clone(),
                key: self.key.clone(),
                token: self.token.clone(),
            };
            handle.spawn(async move {
                let _ = lock.release_inner().await;
            });
        }
    }
}
