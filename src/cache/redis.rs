//! Redis 缓存后端（`cache-redis`）。
//!
//! 多实例部署的必需项：jti 黑名单、会话、缓存一致性都依赖它（见 `docs/02` §12）。
//!
//! 锁使用 `SET key token NX PX ttl` 获取、Lua **compare-and-del** 释放，
//! 保证不会误删他人的锁（见 `docs/03` §4.11）。
//!
//! 所有写入都带 `PX`：**不产生永久键**（永久键会让会话、幂等记录、验证码永不淘汰）。

use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use crate::cache::Cache;
use crate::errors::{AppError, AppResult, ErrorCode};

/// compare-and-del：只有值仍等于自己的 token 才删除。
const DEL_IF_EQ_LUA: &str = r#"
if redis.call("get", KEYS[1]) == ARGV[1] then
    return redis.call("del", KEYS[1])
else
    return 0
end
"#;

/// 原子取走：GET 后 DEL。脚本本身在 Redis 内原子执行。
///
/// 不用 `GETDEL` 是为了兼容 < 6.2 的实例（语义等价）。
const TAKE_LUA: &str = r#"
local v = redis.call("get", KEYS[1])
if v then
    redis.call("del", KEYS[1])
end
return v
"#;

/// Redis 缓存后端。
///
/// 内部使用 `ConnectionManager`（多路复用 + 自动重连），可廉价 `clone`。
#[derive(Clone)]
pub struct RedisCache {
    conn: ConnectionManager,
}

impl std::fmt::Debug for RedisCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisCache").finish_non_exhaustive()
    }
}

impl RedisCache {
    /// 按连接串建立连接。
    ///
    /// `pool_size` 在当前实现中不使用：`ConnectionManager` 为单连接多路复用，
    /// 已足以支撑常规并发；如确需连接池可后续替换为 `deadpool-redis`。
    ///
    /// # Errors
    /// URL 非法或连接失败时返回 `50004`（`CACHE_ERROR`）。
    pub async fn connect(url: &str, _pool_size: u32) -> AppResult<Self> {
        // 建连超时 + 命令响应超时都在 `utils::redis::connect` 里设好
        let conn = crate::utils::redis::connect(url).await.map_err(map_err)?;
        Ok(Self { conn })
    }

    /// 尝试获取锁（`SET NX PX`）。返回 `true` 表示获取成功。
    ///
    /// # Errors
    /// Redis 访问失败时返回 `50004`；`ttl` 为零时返回 `10001`。
    pub async fn try_lock(&self, key: &str, token: &str, ttl: Duration) -> AppResult<bool> {
        self.set_nx(key, token.as_bytes(), ttl).await
    }

    /// 释放锁（Lua compare-and-del）。
    ///
    /// # Errors
    /// Redis 访问失败时返回 `50004`。
    pub async fn release_lock(&self, key: &str, token: &str) -> AppResult<()> {
        self.del_if_eq(key, token.as_bytes()).await?;
        Ok(())
    }

    /// 续期（Lua：仅当值仍等于自己的 token 时 `PEXPIRE`）。返回是否续期成功。
    ///
    /// # Errors
    /// Redis 访问失败时返回 `50004`。
    pub async fn renew_lock(&self, key: &str, token: &str, ttl: Duration) -> AppResult<bool> {
        let mut conn = self.conn.clone();
        let millis = millis(ttl)?;
        let renewed: i64 = redis::Script::new(RENEW_LOCK_LUA)
            .key(key)
            .arg(token)
            .arg(millis)
            .invoke_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(renewed == 1)
    }

    /// 生成单调递增的 fencing token（跨实例有序）。
    ///
    /// # Errors
    /// Redis 访问失败时返回 `50004`。
    pub async fn next_fence(&self, key: &str) -> AppResult<u64> {
        let mut conn = self.conn.clone();
        let fence: u64 = redis::cmd("INCR")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(fence)
    }
}

/// 续期脚本：值匹配才 `PEXPIRE`。
const RENEW_LOCK_LUA: &str = r#"
if redis.call("get", KEYS[1]) == ARGV[1] then
    return redis.call("pexpire", KEYS[1], ARGV[2])
else
    return 0
end
"#;

/// TTL → 毫秒（必须为正）。
fn millis(ttl: Duration) -> AppResult<u64> {
    if ttl.is_zero() {
        return Err(AppError::invalid_param("缓存 TTL 必须大于 0"));
    }
    Ok(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1))
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        let mut conn = self.conn.clone();
        redis::cmd("GET")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(map_err)
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> AppResult<()> {
        let ttl = ttl.ok_or_else(|| AppError::invalid_param("Redis 写入必须带正 TTL"))?;
        let millis = millis(ttl)?;
        let mut conn = self.conn.clone();
        let _: String = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("PX")
            .arg(millis)
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn del(&self, key: &str) -> AppResult<()> {
        let mut conn = self.conn.clone();
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn take(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        let mut conn = self.conn.clone();
        redis::Script::new(TAKE_LUA)
            .key(key)
            .invoke_async(&mut conn)
            .await
            .map_err(map_err)
    }

    async fn set_nx(&self, key: &str, value: &[u8], ttl: Duration) -> AppResult<bool> {
        let millis = millis(ttl)?;
        let mut conn = self.conn.clone();
        let result: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("NX")
            .arg("PX")
            .arg(millis)
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(result.is_some())
    }

    async fn del_if_eq(&self, key: &str, expected: &[u8]) -> AppResult<bool> {
        let mut conn = self.conn.clone();
        let removed: i64 = redis::Script::new(DEL_IF_EQ_LUA)
            .key(key)
            .arg(expected)
            .invoke_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(removed == 1)
    }

    async fn exists(&self, key: &str) -> AppResult<bool> {
        let mut conn = self.conn.clone();
        let count: i64 = redis::cmd("EXISTS")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(count > 0)
    }

    async fn ping(&self) -> AppResult<()> {
        let mut conn = self.conn.clone();
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

/// Redis 错误 → `50004`（不对外泄露细节，内部细节只进日志）。
fn map_err(error: redis::RedisError) -> AppError {
    tracing::error!(%error, "redis 访问失败");
    AppError::biz(ErrorCode::CACHE_ERROR, "缓存访问失败")
}
