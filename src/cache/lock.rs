//! `Lock` trait + 通用助手（key 前缀、默认 TTL、持有者校验，文档 三·10）。
//!
//! - [`MemoryLock`]：进程内锁（默认），单机可用，多实例**不**互斥；
//! - Redis 实现（`SET NX PX` + Lua 校验持有者）见 [`super::redis::RedisLock`]；
//! - [`LockGuard`]：显式 `release` / `extend`；Drop 时 best-effort 释放（进程存活时
//!   由后台任务执行），长任务请主动 `extend` 或把 TTL 设足余量。
//!
//! 未实现自动看门狗续期：这是限流、防重、幂等的公共底座，语义保持最简。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::sections::CacheSettings;

/// 锁操作错误
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("lock backend error: {0}")]
    Backend(String),
}

/// 锁契约：`try_acquire` 返回 `None` 表示锁已被其他持有者占用。
#[async_trait::async_trait]
pub trait Lock: Send + Sync {
    async fn try_acquire(self: Arc<Self>, key: &str, ttl: Duration)
        -> Result<Option<LockGuard>, LockError>;
    /// 释放锁（实现须校验持有者，不会误删他人的锁）。已过期/被抢占时静默成功。
    async fn release(&self, key: &str, token: &str) -> Result<(), LockError>;
    /// 续期（仅当前持锁者可续）。返回 `false` 表示已失去锁，调用方应停止受锁保护的工作。
    async fn extend(&self, key: &str, token: &str, ttl: Duration) -> Result<bool, LockError>;
}

/// 共享锁句柄（存于 CoreState）
pub type LockHandle = Arc<dyn Lock>;

/// 持锁凭证：持有者 token 校验通过后返回。
/// Drop 时 best-effort 释放（TTL 过期兜底），显式 `release()` 成功后跳过 Drop。
pub struct LockGuard {
    pub(crate) backend: Arc<dyn Lock>,
    /// 业务传入的裸 key（后端内部负责加前缀）
    pub(crate) key: String,
    pub(crate) token: String,
    /// release() 成功后置位：Drop 直接跳过（否则 Arc 与 key/token 会随
    /// forget 泄漏——每个请求一次，无上界增长）
    pub(crate) released: bool,
}

impl LockGuard {
    /// 业务侧裸 key（不含后端前缀）
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 释放锁（token 校验）。成功后置 `released`，随 self 正常 Drop——
    /// 不用 `mem::forget`（那会把 Arc 强引用和两个 String 永久泄漏）。
    pub async fn release(mut self) -> Result<(), LockError> {
        match self.backend.release(&self.key, &self.token).await {
            Ok(()) => {
                self.released = true;
                Ok(())
            }
            // 释放失败则照常 Drop，由 Drop 里的 best-effort 重试兜底
            Err(e) => Err(e),
        }
    }

    /// 续期。返回 `false` 表示已失去锁（过期 / 被抢占），调用方应停止工作。
    pub async fn extend(&self, ttl: Duration) -> Result<bool, LockError> {
        self.backend.extend(&self.key, &self.token, ttl).await
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // best-effort：不在 runtime 上下文中时跳过，靠 TTL 过期兜底
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let backend = self.backend.clone();
            let key = self.key.clone();
            let token = self.token.clone();
            handle.spawn(async move {
                let _ = backend.release(&key, &token).await;
            });
        }
    }
}

/// [`Lock`] 的进程内实现：`HashMap<key, (token, 到期时刻)>`，惰性过期。
/// 单机正确；多实例部署必须换 redis 后端（`cache.backend = "redis"`）。
#[derive(Debug, Default)]
pub struct MemoryLock {
    entries: Mutex<HashMap<String, (String, Instant)>>,
}

impl MemoryLock {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl Lock for MemoryLock {
    async fn try_acquire(self: Arc<Self>, key: &str, ttl: Duration) -> Result<Option<LockGuard>, LockError> {
        let token = uuid::Uuid::new_v4().to_string();
        {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // 高基数 key（如 user_id:{id}）用完即忘时，条目只能等同 key 再 acquire
            // 才被覆盖——量级超阈值时顺带清扫过期条目，防 HashMap 无上界增长
            if entries.len() > 1024 {
                let now = Instant::now();
                entries.retain(|_, (_, expires_at)| *expires_at > now);
            }
            match entries.get(key) {
                Some((_, expires_at)) if *expires_at > Instant::now() => return Ok(None),
                _ => {}
            }
            entries.insert(key.to_string(), (token.clone(), Instant::now() + ttl));
        }
        Ok(Some(LockGuard {
            backend: self,
            key: key.to_string(),
            token,
            released: false,
        }))
    }

    async fn release(&self, key: &str, token: &str) -> Result<(), LockError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.get(key).map(|(t, _)| t.as_str()) == Some(token) {
            entries.remove(key);
        }
        Ok(())
    }

    async fn extend(&self, key: &str, token: &str, ttl: Duration) -> Result<bool, LockError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match entries.get_mut(key) {
            Some((t, expires_at)) if t == token => {
                *expires_at = Instant::now() + ttl;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// 按 `[cache]` 配置构建锁实例（与 cache 后端配对；App 装配时自动调用）
pub fn build_lock(settings: &CacheSettings) -> Result<LockHandle, super::CacheError> {
    match settings.backend.as_str() {
        "memory" => Ok(Arc::new(MemoryLock::new())),
        "redis" => {
            #[cfg(feature = "cache-redis")]
            {
                Ok(Arc::new(super::redis::RedisLock::new(&settings.redis)?))
            }
            #[cfg(not(feature = "cache-redis"))]
            {
                Err(super::CacheError::FeatureDisabled("redis".to_string()))
            }
        }
        other => Err(super::CacheError::UnknownBackend(other.to_string())),
    }
}
