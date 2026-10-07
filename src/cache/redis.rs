//! Redis 缓存后端（feature = "cache-redis"）：deadpool-redis 连接池。
//! 多实例部署使用；分布式锁（`SET NX PX`）实现同样在本文件。

use std::time::Duration; // 引入时长类型，用于 TTL 参数

use deadpool_redis::{Config as PoolConfig, Runtime}; // 引入连接池配置与 Tokio 运行时标识

use super::lock::{Lock, LockError, LockGuard}; // 引入锁契约与错误、守卫类型
use super::{Cache, CacheError}; // 引入缓存契约与错误类型
use crate::config::sections::CacheRedisSettings; // 引入 Redis 配置节

/// 共享 Redis 连接池（cache 后端与 queue-redis 后端可分别建池；同 url 亦互不影响）
#[derive(Debug, Clone)] // 派生调试与克隆（池内部为 Arc）
pub struct RedisPool { // Redis 连接池封装
    pub pool: deadpool_redis::Pool, // 底层 deadpool 连接池
}

impl RedisPool { // 连接池构造实现块
    pub fn from_cache_settings(settings: &CacheRedisSettings) -> Result<Self, CacheError> { // 依据配置构造连接池
        Self::from_url(&settings.url, settings.pool_size).map_err(Into::into) // 复用 from_url 并把错误转成 CacheError
    }

    pub fn from_url(url: &str, pool_size: u32) -> Result<Self, deadpool_redis::CreatePoolError> { // 依据 URL 与池大小构造
        let mut pool_cfg = PoolConfig::from_url(url.to_string()); // 从 URL 生成基础池配置
        if pool_size > 0 { // 仅当显式指定了池大小时覆盖
            pool_cfg.pool = Some(deadpool_redis::PoolConfig { // 设置自定义池参数
                max_size: pool_size as usize, // 最大连接数
                ..Default::default() // 其余字段用默认值
            });
        }
        Ok(Self { // 构造连接池封装
            pool: pool_cfg.create_pool(Some(Runtime::Tokio1))?, // 在 Tokio1 运行时上创建连接池
        })
    }
}

#[derive(Debug, Clone)] // 派生调试与克隆
pub struct RedisCache { // Redis 缓存后端实现
    pool: deadpool_redis::Pool, // 共享连接池
}

impl RedisCache { // Redis 缓存实现块
    pub fn new(settings: &CacheRedisSettings) -> Result<Self, CacheError> { // 依据配置构造 Redis 缓存
        Ok(Self { // 构造实例
            pool: RedisPool::from_cache_settings(settings)?.pool, // 由配置建池并取出池句柄
        })
    }

    pub(crate) async fn conn( // 从池中取一条连接的内部辅助方法
        &self, // 缓存实例引用
    ) -> Result<deadpool_redis::Connection, deadpool_redis::PoolError> { // 返回连接或池错误
        self.pool.get().await // 异步取连接
    }
}

#[async_trait::async_trait] // 启用 async_trait 以实现异步 trait
impl Cache for RedisCache { // 为 Redis 缓存实现 Cache 契约
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> { // 读取缓存值
        let mut conn = self.conn().await?; // 取一条连接
        Ok(deadpool_redis::redis::cmd("GET") // 构造 GET 命令
            .arg(key) // 指定键
            .query_async::<Option<String>>(&mut conn) // 执行并解析为可选字符串
            .await?) // 等待结果
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> { // 写入缓存值
        let mut conn = self.conn().await?; // 取一条连接
        let mut cmd = deadpool_redis::redis::cmd("SET"); // 构造 SET 命令
        cmd.arg(key).arg(value); // 设置键与值
        if let Some(d) = ttl { // 若指定了 TTL
            cmd.arg("EX").arg(d.as_secs().max(1)); // 追加 EX 秒数（至少 1 秒）
        }
        cmd.query_async::<()>(&mut conn).await?; // 执行命令
        Ok(()) // 返回成功
    }

    async fn del(&self, key: &str) -> Result<(), CacheError> { // 删除缓存键
        let mut conn = self.conn().await?; // 取一条连接
        deadpool_redis::redis::cmd("DEL") // 构造 DEL 命令
            .arg(key) // 指定键
            .query_async::<()>(&mut conn) // 执行命令
            .await?; // 等待结果
        Ok(()) // 返回成功
    }

    /// 原生 `INCRBY`：多实例共享同一计数（限流 / 幂等计数的推荐后端）
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError> { // 对计数键原子累加
        let mut conn = self.conn().await?; // 取一条连接
        Ok(deadpool_redis::redis::cmd("INCRBY") // 构造 INCRBY 命令
            .arg(key) // 指定键
            .arg(delta) // 指定增量
            .query_async::<i64>(&mut conn) // 执行并解析为整数
            .await?) // 等待结果
    }

    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> { // 重设键的 TTL
        let mut conn = self.conn().await?; // 取一条连接
        let touched: i64 = match ttl { // 依据 ttl 选择命令
            Some(d) if d > Duration::ZERO => { // 正时长走 EXPIRE
                deadpool_redis::redis::cmd("EXPIRE") // 构造 EXPIRE 命令
                    .arg(key) // 指定键
                    .arg(d.as_secs().max(1)) // 秒数（至少 1 秒）
                    .query_async(&mut conn) // 执行命令
                    .await? // 等待结果
            }
            _ => { // 其余情况（None/ZERO）走 PERSIST
                deadpool_redis::redis::cmd("PERSIST") // 构造 PERSIST 命令
                    .arg(key) // 指定键
                    .query_async(&mut conn) // 执行命令
                    .await? // 等待结果
            }
        };
        Ok(touched == 1) // 命令返回 1 表示键存在且被处理
    }

    async fn ping(&self) -> Result<(), CacheError> { // 探活 Redis 后端
        let mut conn = self.conn().await?; // 取一条连接
        deadpool_redis::redis::cmd("PING") // 构造 PING 命令
            .query_async::<()>(&mut conn) // 执行命令
            .await?; // 等待结果
        Ok(()) // 返回成功
    }
}

// ---------- 分布式锁（SET NX PX + Lua 持有者校验） ----------

const RELEASE_LUA: &str = // 释放锁的 Lua 脚本：仅持有者可删
    "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end";

const EXTEND_LUA: &str = // 续期锁的 Lua 脚本：仅持有者可续期
    "if redis.call('get', KEYS[1]) == ARGV[1] then redis.call('set', KEYS[1], ARGV[1], 'PX', ARGV[2]) return 1 else return 0 end";

#[derive(Debug, Clone)] // 派生调试与克隆
pub struct RedisLock { // 基于 Redis 的分布式锁实现
    pool: deadpool_redis::Pool, // 共享连接池
    key_prefix: String, // 锁键前缀，避免与其他业务键冲突
}

impl RedisLock { // 分布式锁构造实现块
    pub fn new(settings: &CacheRedisSettings) -> Result<Self, CacheError> { // 依据配置构造分布式锁
        Ok(Self { // 构造实例
            pool: RedisPool::from_cache_settings(settings)?.pool, // 由配置建池并取出池句柄
            key_prefix: if settings.lock_prefix.is_empty() { // 前缀为空时用默认值
                "core-rs:lock:".to_string() // 默认锁键前缀
            } else { // 否则使用配置值
                settings.lock_prefix.clone() // 克隆配置的前缀
            },
        })
    }

    fn full_key(&self, key: &str) -> String { // 拼接带前缀的完整锁键
        format!("{}{}", self.key_prefix, key) // 前缀 + 业务裸 key
    }
}

#[async_trait::async_trait] // 启用 async_trait 以实现异步 trait
impl Lock for RedisLock { // 为 RedisLock 实现 Lock 契约
    async fn try_acquire(self: std::sync::Arc<Self>, key: &str, ttl: Duration) -> Result<Option<LockGuard>, LockError> { // 尝试加锁
        // ttl 为零（含亚毫秒）语义 = 不锁；EX 是秒粒度，亚秒 TTL 会被放大成 1 秒
        if ttl < Duration::from_millis(1) { // 非正时长视为不锁
            return Ok(None); // 直接返回未获取
        }
        let token = uuid::Uuid::new_v4().to_string(); // 生成本次持锁唯一 token
        let full = self.full_key(key); // 计算带前缀的完整键
        let mut conn = self // 从池取连接
            .pool // 访问连接池
            .get() // 取一条连接
            .await // 等待获取
            .map_err(|e| LockError::Backend(e.to_string()))?; // 取连接失败转成锁后端错误
        let ok: Option<String> = deadpool_redis::redis::cmd("SET") // 构造 SET 命令
            .arg(&full) // 指定完整键
            .arg(&token) // 值为 token
            .arg("NX") // 仅当键不存在时设置
            // PX（毫秒）：EX 会把 200ms 放大成 1s，锁的互斥窗口与调用方预期不符
            .arg("PX") // 指定毫秒级过期
            .arg(ttl.as_millis().max(1) as u64) // TTL 毫秒数（至少 1）
            .query_async(&mut conn) // 执行命令
            .await // 等待结果
            .map_err(|e| LockError::Backend(e.to_string()))?; // 命令失败转成锁后端错误
        Ok(ok.map(|_| LockGuard { // 设置成功则返回守卫
            backend: self, // 守卫持有锁后端的 Arc
            // guard 存裸 key：release/extend 内部统一过 full_key，两后端契约一致
            key: key.to_string(), // 存业务裸 key
            token, // 存本次持锁 token
            released: false, // 初始未释放
        }))
    }

    async fn release(&self, key: &str, token: &str) -> Result<(), LockError> { // 释放锁
        let mut conn = self // 从池取连接
            .pool // 访问连接池
            .get() // 取一条连接
            .await // 等待获取
            .map_err(|e| LockError::Backend(e.to_string()))?; // 取连接失败转成锁后端错误
        // key 与 try_acquire 同一契约：业务裸 key，这里补前缀
        deadpool_redis::redis::Script::new(RELEASE_LUA) // 装载释放脚本
            .key(self.full_key(key)) // 传入完整键
            .arg(token) // 传入持有者 token
            .invoke_async::<()>(&mut conn) // 执行脚本
            .await // 等待结果
            .map_err(|e| LockError::Backend(e.to_string()))?; // 脚本失败转成锁后端错误
        Ok(()) // 返回成功
    }

    async fn extend(&self, key: &str, token: &str, ttl: Duration) -> Result<bool, LockError> { // 续期锁
        if ttl < Duration::from_millis(1) { // 非正时长视为无法续期
            return Ok(false); // 返回续期失败
        }
        let mut conn = self // 从池取连接
            .pool // 访问连接池
            .get() // 取一条连接
            .await // 等待获取
            .map_err(|e| LockError::Backend(e.to_string()))?; // 取连接失败转成锁后端错误
        let renewed: i64 = deadpool_redis::redis::Script::new(EXTEND_LUA) // 装载续期脚本
            .key(self.full_key(key)) // 传入完整键
            .arg(token) // 传入持有者 token
            .arg(ttl.as_millis().max(1) as u64) // 新 TTL 毫秒数（至少 1）
            .invoke_async(&mut conn) // 执行脚本
            .await // 等待结果
            .map_err(|e| LockError::Backend(e.to_string()))?; // 脚本失败转成锁后端错误
        Ok(renewed == 1) // 返回 1 表示续期成功
    }
}

