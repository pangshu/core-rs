//! Redis 限流存储（`ratelimit-redis`）：多实例共享计数。
//!
//! 语义：**固定窗口**（`INCR` + 首次 `PEXPIRE`），与 `memory` 的 GCRA 不完全等价——
//!
//! | 实现 | 语义 | 特点 |
//! |---|---|---|
//! | `memory`（governor GCRA） | 令牌桶式平滑限流 | 无窗口边界突刺，但状态在进程内 |
//! | `redis`（本实现） | 固定窗口计数 | 多实例共享；窗口交界处可能出现 2× 突刺 |
//!
//! 需要严格平滑时用 `memory`（单实例）或把窗口调小以稀释边界效应。
//!
//! 键：`{namespace}:ratelimit[:{rule}]:{key}`，`namespace` 取 `[cache].namespace`
//! （**不再硬编码**：否则共用同一个 Redis DB 的多个应用会互相踩桶——A 的流量
//! 限掉 B 的用户，按 namespace 清理/隔离也一并失效），连接复用
//! `[cache].redis.url`。
//!
//! ## 故障策略：fail-open，但降级到进程内兜底
//!
//! Redis 不可用时**放行**（限流是保护性能力，存储故障导致全站 429 的伤害远大于
//! 短暂失去限流），但放行前会再过一道进程内的 GCRA 兜底桶：多副本时实际配额是
//! `limit × 副本数`（各副本独立计数），比完全不限流好得多，也不会让攻击者靠
//! 「把 Redis 打挂」换来无限配额。
//!
//! 告警按 [`ThrottledWarn`] 聚合——否则 Redis 一抖动，每请求一条 `warn` 会把
//! 日志系统先压垮。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::ratelimit::store::{DEFAULT_MAX_KEYS, MemoryStore, RateLimitStore};
use crate::ratelimit::throttle::ThrottledWarn;

/// 计数 + 首次设置过期 + 读取剩余 TTL，一次往返完成。
///
/// 返回 `{current, pttl}`。
///
/// **`ttl < 0` 时必须补 `PEXPIRE`**：只判 `current == 1` 的写法有个致命缺陷——
/// 键一旦在某一刻丢了 TTL（主从切换时脚本效果未完整复制、RDB 恢复、运维
/// `COPY`/`RESTORE`/`PERSIST`、旧版本写入），`current` 就恒大于 1，`PEXPIRE`
/// 再也不会被执行，于是**该键永不过期、该 IP/主体永久 429**，且无法自愈。
const CHECK_LUA: &str = r"
local current = redis.call('INCR', KEYS[1])
local ttl = redis.call('PTTL', KEYS[1])
if current == 1 or ttl < 0 then
    redis.call('PEXPIRE', KEYS[1], ARGV[1])
    ttl = tonumber(ARGV[1])
end
return {current, ttl}
";

/// Redis 限流存储。
#[derive(Clone)]
pub struct RedisRateLimitStore {
    conn: ConnectionManager,
    /// `{namespace}:ratelimit`，规则桶在它后面再挂一段
    base: String,
    prefix: String,
    limit: u32,
    window: Duration,
    /// fail-open 时的进程内兜底（多副本时实际配额 = `limit × 副本数`）
    fallback: Arc<MemoryStore>,
    warn: Arc<ThrottledWarn>,
}

impl std::fmt::Debug for RedisRateLimitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisRateLimitStore")
            .field("prefix", &self.prefix)
            .field("limit", &self.limit)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl RedisRateLimitStore {
    /// 建立连接。
    ///
    /// `namespace` 取 `[cache].namespace`（共用 Redis DB 时用于隔离键空间）。
    ///
    /// # Errors
    /// URL 非法或连接失败时返回 `50004`。
    pub async fn connect(
        url: &str,
        namespace: &str,
        limit: u32,
        window: Duration,
    ) -> AppResult<Self> {
        // 建连超时 + 命令响应超时都在 `utils::redis::connect` 里设好
        let conn = crate::utils::redis::connect(url).await.map_err(map_err)?;
        let limit = limit.max(1);
        let window = if window.is_zero() {
            Duration::from_secs(1)
        } else {
            window
        };
        let base = format!("{namespace}:ratelimit");
        Ok(Self {
            conn,
            prefix: base.clone(),
            base,
            limit,
            window,
            fallback: Arc::new(MemoryStore::with_max_keys(limit, window, DEFAULT_MAX_KEYS)),
            warn: Arc::new(ThrottledWarn::new(Duration::from_secs(10))),
        })
    }

    /// 从已有连接派生一个**独立命名空间**的桶（`[[ratelimit.rules]]` 用）。
    ///
    /// 复用同一条 `ConnectionManager`（内部是 `Arc`），不会新建连接。
    #[must_use]
    pub fn derive(&self, rule: &str, limit: u32, window: Duration) -> Self {
        let limit = limit.max(1);
        let window = if window.is_zero() {
            Duration::from_secs(1)
        } else {
            window
        };
        Self {
            conn: self.conn.clone(),
            prefix: format!("{}:{rule}", self.base),
            base: self.base.clone(),
            limit,
            window,
            fallback: Arc::new(MemoryStore::with_max_keys(limit, window, DEFAULT_MAX_KEYS)),
            warn: Arc::clone(&self.warn),
        }
    }

    fn key(&self, key: &str) -> String {
        format!("{}:{}", self.prefix, key)
    }

    /// Redis 访问失败时的兜底：进程内 GCRA 再判一次。
    fn degrade(&self, key: &str, error: &redis::RedisError) -> Result<(), Duration> {
        // 告警按 10s 聚合：Redis 抖动时逐请求 warn 会先把日志系统压垮
        if let Some(suppressed) = self.warn.hit() {
            tracing::warn!(
                %error,
                suppressed,
                "Redis 限流访问失败，降级到进程内兜底桶（多副本时实际配额为 limit × 副本数）"
            );
        }
        self.fallback.check_now(key)
    }
}

#[async_trait]
impl RateLimitStore for RedisRateLimitStore {
    fn window(&self) -> Duration {
        self.window
    }

    async fn check(&self, key: &str) -> Result<(), Duration> {
        let mut conn = self.conn.clone();
        let millis = u64::try_from(self.window.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);

        let result: Vec<i64> = match redis::Script::new(CHECK_LUA)
            .key(self.key(key))
            .arg(millis)
            .invoke_async(&mut conn)
            .await
        {
            Ok(result) => result,
            // **fail-open**（带进程内兜底）：限流存储故障不应把整个服务变成 429。
            Err(error) => return self.degrade(key, &error),
        };

        let current = result.first().copied().unwrap_or(0);
        let pttl = result.get(1).copied().unwrap_or(-1);

        if current > i64::from(self.limit) {
            // `pttl < 0`（键无过期）时回退到整窗，避免返回 0 让客户端立刻重试
            let wait = if pttl > 0 {
                Duration::from_millis(u64::try_from(pttl).unwrap_or(1))
            } else {
                self.window
            };
            return Err(wait);
        }
        Ok(())
    }
}

fn map_err(error: redis::RedisError) -> AppError {
    tracing::error!(%error, "Redis 限流连接失败");
    AppError::biz(ErrorCode::CACHE_ERROR, "限流存储不可用")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lua 脚本必须同时覆盖「首次计数」与「键丢失 TTL」两种需要设置过期的场景。
    ///
    /// 只判 `current == 1` 会让丢过 TTL 的键永不过期（该来源被永久 429）。
    #[test]
    fn should_repair_missing_ttl_in_lua() {
        assert!(
            CHECK_LUA.contains("current == 1 or ttl < 0"),
            "脚本必须对 ttl < 0 的键补 PEXPIRE"
        );
        assert!(CHECK_LUA.contains("PEXPIRE"), "必须设置过期");
        // 先取 PTTL 再判断，避免多一次往返
        assert!(CHECK_LUA.contains("PTTL"));
    }
}
