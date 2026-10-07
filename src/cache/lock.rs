//! `Lock` trait + 通用助手（key 前缀、默认 TTL、持有者校验，文档 三·10）。
//!
//! - [`MemoryLock`]：进程内锁（默认），单机可用，多实例**不**互斥；
//! - Redis 实现（`SET NX PX` + Lua 校验持有者）见 [`super::redis::RedisLock`]；
//! - [`LockGuard`]：显式 `release` / `extend`；Drop 时 best-effort 释放（进程存活时
//!   由后台任务执行），长任务请主动 `extend` 或把 TTL 设足余量。
//!
//! 未实现自动看门狗续期：这是限流、防重、幂等的公共底座，语义保持最简。

use std::collections::HashMap; // 引入哈希表，存储进程内锁条目
use std::sync::{Arc, Mutex}; // 引入原子引用计数与互斥锁
use std::time::{Duration, Instant}; // 引入时长与单调时钟，用于 TTL 与过期判定

use crate::config::sections::CacheSettings; // 引入 [cache] 配置节，构建锁时读取

/// 锁操作错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 与 thiserror 错误实现
pub enum LockError { // 锁统一错误枚举
    #[error("lock backend error: {0}")] // 错误文案：锁后端错误
    Backend(String), // 记录后端错误信息
}

/// 锁契约：`try_acquire` 返回 `None` 表示锁已被其他持有者占用。
#[async_trait::async_trait] // 启用 async_trait，使 trait 可含 async 方法
pub trait Lock: Send + Sync { // 锁后端统一契约，要求线程安全
    async fn try_acquire(self: Arc<Self>, key: &str, ttl: Duration) // 尝试加锁，成功返回守卫
        -> Result<Option<LockGuard>, LockError>; // 返回 None 表示锁被占用
    /// 释放锁（实现须校验持有者，不会误删他人的锁）。已过期/被抢占时静默成功。
    async fn release(&self, key: &str, token: &str) -> Result<(), LockError>; // 释放锁，需持有者 token 校验
    /// 续期（仅当前持锁者可续）。返回 `false` 表示已失去锁，调用方应停止受锁保护的工作。
    async fn extend(&self, key: &str, token: &str, ttl: Duration) -> Result<bool, LockError>; // 续期锁，返回是否仍持有
}

/// 共享锁句柄（存于 CoreState）
pub type LockHandle = Arc<dyn Lock>; // 类型别名：线程安全的锁 trait 对象句柄

/// 持锁凭证：持有者 token 校验通过后返回。
/// Drop 时 best-effort 释放（TTL 过期兜底），显式 `release()` 成功后跳过 Drop。
pub struct LockGuard { // 锁守卫，代表一次成功持锁
    pub(crate) backend: Arc<dyn Lock>, // 持锁的后端句柄
    /// 业务传入的裸 key（后端内部负责加前缀）
    pub(crate) key: String, // 业务裸 key
    pub(crate) token: String, // 本次持锁唯一 token
    /// release() 成功后置位：Drop 直接跳过（否则 Arc 与 key/token 会随
    /// forget 泄漏——每个请求一次，无上界增长）
    pub(crate) released: bool, // 是否已显式释放
}

impl LockGuard { // 锁守卫实现块
    /// 业务侧裸 key（不含后端前缀）
    pub fn key(&self) -> &str { // 返回业务裸 key
        &self.key // 借用内部 key 字段
    }

    /// 释放锁（token 校验）。成功后置 `released`，随 self 正常 Drop——
    /// 不用 `mem::forget`（那会把 Arc 强引用和两个 String 永久泄漏）。
    pub async fn release(mut self) -> Result<(), LockError> { // 显式释放锁
        match self.backend.release(&self.key, &self.token).await { // 调用后端释放
            Ok(()) => { // 释放成功
                self.released = true; // 标记已释放，Drop 时跳过
                Ok(()) // 返回成功
            }
            // 释放失败则照常 Drop，由 Drop 里的 best-effort 重试兜底
            Err(e) => Err(e), // 透传释放错误
        }
    }

    /// 续期。返回 `false` 表示已失去锁（过期 / 被抢占），调用方应停止工作。
    pub async fn extend(&self, ttl: Duration) -> Result<bool, LockError> { // 续期当前锁
        self.backend.extend(&self.key, &self.token, ttl).await // 委托后端按 token 续期
    }
}

impl Drop for LockGuard { // 为锁守卫实现析构
    fn drop(&mut self) { // 守卫离开作用域时执行
        if self.released { // 已显式释放则无需再处理
            return; // 直接返回
        }
        // best-effort：不在 runtime 上下文中时跳过，靠 TTL 过期兜底
        if let Ok(handle) = tokio::runtime::Handle::try_current() { // 仅在 Tokio 运行时上下文中尝试
            let backend = self.backend.clone(); // 克隆后端 Arc 供异步任务持有
            let key = self.key.clone(); // 克隆 key 供异步任务持有
            let token = self.token.clone(); // 克隆 token 供异步任务持有
            handle.spawn(async move { // 派生后台任务异步释放
                let _ = backend.release(&key, &token).await; // 忽略释放结果（best-effort）
            });
        }
    }
}

/// [`Lock`] 的进程内实现：`HashMap<key, (token, 到期时刻)>`，惰性过期。
/// 单机正确；多实例部署必须换 redis 后端（`cache.backend = "redis"`）。
#[derive(Debug, Default)] // 派生调试与默认构造
pub struct MemoryLock { // 进程内锁实现
    entries: Mutex<HashMap<String, (String, Instant)>>, // 锁表：键 -> (token, 到期时刻)
}

impl MemoryLock { // 进程内锁实现块
    pub fn new() -> Self { // 构造空的进程内锁
        Self::default() // 使用派生的默认值
    }
}

#[async_trait::async_trait] // 启用 async_trait 以实现异步 trait
impl Lock for MemoryLock { // 为 MemoryLock 实现 Lock 契约
    async fn try_acquire(self: Arc<Self>, key: &str, ttl: Duration) -> Result<Option<LockGuard>, LockError> { // 尝试加锁
        let token = uuid::Uuid::new_v4().to_string(); // 生成本次持锁唯一 token
        {
            let mut entries = self // 获取锁表
                .entries // 访问内部锁表
                .lock() // 获取互斥锁
                .unwrap_or_else(std::sync::PoisonError::into_inner); // 锁被 poison 时取回内部值
            // 高基数 key（如 user_id:{id}）用完即忘时，条目只能等同 key 再 acquire
            // 才被覆盖——量级超阈值时顺带清扫过期条目，防 HashMap 无上界增长
            if entries.len() > 1024 { // 条目超阈值时顺带清扫
                let now = Instant::now(); // 取当前时刻供批量比较
                entries.retain(|_, (_, expires_at)| *expires_at > now); // 仅保留未过期条目
            }
            match entries.get(key) { // 检查键是否已被占用
                Some((_, expires_at)) if *expires_at > Instant::now() => return Ok(None), // 未过期则加锁失败
                _ => {} // 不存在或已过期则继续
            }
            entries.insert(key.to_string(), (token.clone(), Instant::now() + ttl)); // 写入新持锁记录
        }
        Ok(Some(LockGuard { // 加锁成功，返回守卫
            backend: self, // 守卫持有锁后端 Arc
            key: key.to_string(), // 存业务裸 key
            token, // 存本次持锁 token
            released: false, // 初始未释放
        }))
    }

    async fn release(&self, key: &str, token: &str) -> Result<(), LockError> { // 释放锁
        let mut entries = self // 获取锁表
            .entries // 访问内部锁表
            .lock() // 获取互斥锁
            .unwrap_or_else(std::sync::PoisonError::into_inner); // 锁被 poison 时取回内部值
        if entries.get(key).map(|(t, _)| t.as_str()) == Some(token) { // 仅当 token 匹配时才删除
            entries.remove(key); // 移除持锁记录
        }
        Ok(()) // 返回成功（不匹配时静默成功）
    }

    async fn extend(&self, key: &str, token: &str, ttl: Duration) -> Result<bool, LockError> { // 续期锁
        let mut entries = self // 获取锁表
            .entries // 访问内部锁表
            .lock() // 获取互斥锁
            .unwrap_or_else(std::sync::PoisonError::into_inner); // 锁被 poison 时取回内部值
        match entries.get_mut(key) { // 查找持锁记录
            Some((t, expires_at)) if t == token => { // token 匹配则续期
                *expires_at = Instant::now() + ttl; // 更新到期时刻
                Ok(true) // 返回续期成功
            }
            _ => Ok(false), // 无记录或 token 不匹配则续期失败
        }
    }
}

/// 按 `[cache]` 配置构建锁实例（与 cache 后端配对；App 装配时自动调用）
pub fn build_lock(settings: &CacheSettings) -> Result<LockHandle, super::CacheError> { // 依据配置选择并构造锁后端
    match settings.backend.as_str() { // 按 backend 字段分派
        "memory" => Ok(Arc::new(MemoryLock::new())), // 内存后端返回进程内锁
        "redis" => { // Redis 后端分支
            #[cfg(feature = "cache-redis")] // 开启 Redis feature 时使用下面实现
            {
                Ok(Arc::new(super::redis::RedisLock::new(&settings.redis)?)) // 用 Redis 配置构造分布式锁
            }
            #[cfg(not(feature = "cache-redis"))] // 未开启 Redis feature 时使用下面实现
            {
                Err(super::CacheError::FeatureDisabled("redis".to_string())) // 返回 feature 未启用错误
            }
        }
        other => Err(super::CacheError::UnknownBackend(other.to_string())), // 其余值视为未知后端
    }
}
