//! 分布式锁。
//!
//! - **Redis 后端**（`cache-redis`）：`SET key token NX PX ttl` 获取，
//!   **Lua compare-and-del** 释放——锁值带随机 token，避免超时后误删他人锁
//!   （见 `docs/03` §4.11）。
//! - **内存后端**：进程内互斥（单实例语义），键空间与 Redis 一致；注册表记录
//!   到期时刻，惰性清理，避免 guard 泄漏导致永久占锁。
//!
//! 释放走 [`Drop`]；Redis 释放需要 I/O，故在后台任务中执行。
//!
//! **token 是密码学随机的 UUIDv4**：可预测的 token 会让「compare-and-del」退化成
//! 摆设——任何能读到 token 的组件都能伪造释放。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::errors::{AppError, AppResult};

/// 进程内锁条目：持有者 token + 到期时刻。
#[derive(Debug, Clone)]
pub(crate) struct LockEntry {
    /// 持有者 token。
    pub(crate) token: String,
    /// 到期时刻（惰性清理依据）。
    pub(crate) expires_at: Instant,
}

/// 进程内锁注册表：`key -> LockEntry`。
pub(crate) type LockRegistry = Arc<Mutex<HashMap<String, LockEntry>>>;

/// 已获取的锁；**drop 即释放**。
pub struct DistributedLock {
    key: String,
    token: String,
    release: Release,
}

enum Release {
    /// 进程内锁：直接从注册表移除。
    Memory(LockRegistry),
    /// Redis 锁：drop 时在后台任务里执行 compare-and-del。
    #[cfg(feature = "cache-redis")]
    Redis(Arc<crate::cache::redis::RedisCache>),
}

impl std::fmt::Debug for DistributedLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // token 只露前 8 位：完整 token 足以伪造 compare-and-del
        let short = self.token.get(..8).unwrap_or(&self.token);
        f.debug_struct("DistributedLock")
            .field("key", &self.key)
            .field("token", &format_args!("{short}…"))
            .finish_non_exhaustive()
    }
}

impl DistributedLock {
    pub(crate) fn memory(registry: LockRegistry, key: String, token: String) -> Self {
        Self {
            key,
            token,
            release: Release::Memory(registry),
        }
    }

    #[cfg(feature = "cache-redis")]
    pub(crate) fn redis(
        backend: Arc<crate::cache::redis::RedisCache>,
        key: String,
        token: String,
    ) -> Self {
        Self {
            key,
            token,
            release: Release::Redis(backend),
        }
    }

    /// 锁键（已含命名空间与 `l:` 分类段）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 锁值 token（随机；排查用，**不要写进日志或响应**）。
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// 续期。返回 `false` 表示锁已不属于自己（超时被他人获取）。
    ///
    /// 长任务应周期性调用，避免 TTL 到期后与另一实例并发执行。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`；`ttl` 为零时返回 `10001`。
    pub async fn renew(&self, ttl: Option<Duration>, default_ttl: Duration) -> AppResult<bool> {
        let ttl = effective_ttl(ttl, default_ttl)?;
        match &self.release {
            Release::Memory(registry) => {
                let mut map = registry.lock().unwrap_or_else(|error| error.into_inner());
                match map.get_mut(&self.key) {
                    Some(entry) if entry.token == self.token => {
                        entry.expires_at = Instant::now() + ttl;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            #[cfg(feature = "cache-redis")]
            Release::Redis(backend) => backend.renew_lock(&self.key, &self.token, ttl).await,
        }
    }
}

impl Drop for DistributedLock {
    fn drop(&mut self) {
        match &self.release {
            Release::Memory(registry) => {
                // 仅当 token 仍是自己写入的那个才移除（防御性，正常不会冲突）。
                let mut map = registry.lock().unwrap_or_else(|error| error.into_inner());
                if map.get(&self.key).is_some_and(|e| e.token == self.token) {
                    map.remove(&self.key);
                }
            }
            #[cfg(feature = "cache-redis")]
            Release::Redis(backend) => {
                let backend = backend.clone();
                let key = self.key.clone();
                let token = self.token.clone();
                // Drop 不能 await：交给后台任务。无运行时（如进程退出）则放弃释放，
                // 由锁 TTL 兜底。
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = backend.release_lock(&key, &token).await {
                                tracing::warn!(%error, %key, "释放分布式锁失败，等待 TTL 过期");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!(%key, "无异步运行时，跳过锁释放，等待 TTL 过期");
                    }
                }
            }
        }
    }
}

/// 尝试在进程内注册表中占位（带惰性过期清理）。
pub(crate) fn try_acquire_memory(
    registry: &LockRegistry,
    key: &str,
    token: &str,
    ttl: Duration,
) -> bool {
    let mut map = registry.lock().unwrap_or_else(|error| error.into_inner());
    let now = Instant::now();
    if let Some(existing) = map.get(key) {
        if existing.expires_at > now {
            return false;
        }
        // 已过期：允许抢占（与 Redis TTL 语义一致）
        map.remove(key);
    }
    map.insert(
        key.to_string(),
        LockEntry {
            token: token.to_string(),
            expires_at: now + ttl,
        },
    );
    true
}

/// 生成锁 token：密码学随机的 UUIDv4（122 bit 熵）。
pub(crate) fn next_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 锁的 TTL：显式值优先，否则用默认值；**零值一律拒绝**（`PX 1` 等于没加锁）。
///
/// # Errors
/// `ttl` 与 `default_ttl` 都为零时返回 `10001`。
pub(crate) fn effective_ttl(ttl: Option<Duration>, default_ttl: Duration) -> AppResult<Duration> {
    let ttl = match ttl {
        Some(ttl) if !ttl.is_zero() => ttl,
        _ => default_ttl,
    };
    if ttl.is_zero() {
        return Err(AppError::invalid_param("锁 TTL 必须大于 0"));
    }
    Ok(ttl)
}

/// 供测试与调用方断言：锁是否仍被占用（内存后端）。
#[cfg(test)]
pub(crate) fn is_locked(registry: &LockRegistry, key: &str) -> bool {
    registry
        .lock()
        .map(|m| m.contains_key(key))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> LockRegistry {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn should_generate_unique_random_tokens() {
        let a = next_token();
        let b = next_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "应为 UUID 字符串");
    }

    #[test]
    fn should_reject_zero_ttl() {
        assert!(effective_ttl(None, Duration::ZERO).is_err());
        assert!(effective_ttl(Some(Duration::ZERO), Duration::ZERO).is_err());
        assert_eq!(
            effective_ttl(None, Duration::from_secs(10)).unwrap(),
            Duration::from_secs(10)
        );
        assert_eq!(
            effective_ttl(Some(Duration::from_secs(3)), Duration::from_secs(10)).unwrap(),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn should_acquire_once_and_release_on_drop() {
        let registry = registry();
        let token = next_token();
        assert!(try_acquire_memory(
            &registry,
            "k",
            &token,
            Duration::from_secs(10)
        ));
        assert!(
            !try_acquire_memory(&registry, "k", "other", Duration::from_secs(10)),
            "已被占用"
        );

        {
            let _guard = DistributedLock::memory(registry.clone(), "k".to_string(), token);
            assert!(is_locked(&registry, "k"));
        }
        assert!(!is_locked(&registry, "k"), "drop 后应释放");
    }

    #[test]
    fn should_not_release_someone_elses_lock() {
        let registry = registry();
        assert!(try_acquire_memory(
            &registry,
            "k",
            "owner",
            Duration::from_secs(10)
        ));
        {
            // 模拟「自己的锁已超时，被他人重新获取」
            let _guard =
                DistributedLock::memory(registry.clone(), "k".to_string(), "stale".to_string());
        }
        assert!(is_locked(&registry, "k"), "不应删除他人的锁");
    }

    #[test]
    fn should_allow_takeover_after_expiry() {
        let registry = registry();
        assert!(try_acquire_memory(
            &registry,
            "k",
            "first",
            Duration::from_millis(1)
        ));
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            try_acquire_memory(&registry, "k", "second", Duration::from_secs(10)),
            "过期后应可抢占"
        );
    }

    #[test]
    fn should_hide_token_in_debug() {
        let registry = registry();
        let token = next_token();
        let guard = DistributedLock::memory(registry, "k".to_string(), token.clone());
        let text = format!("{guard:?}");
        assert!(!text.contains(&token), "Debug 不应泄露完整 token");
        assert!(!text.contains("HashMap"), "Debug 不应打印锁注册表");
    }

    #[test]
    fn should_recover_from_poisoned_registry() {
        let registry = registry();
        let clone = registry.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock().unwrap();
            panic!("poison the registry");
        })
        .join();
        assert!(
            try_acquire_memory(&registry, "k", "token", Duration::from_secs(10)),
            "注册表中毒后仍应能取锁"
        );
    }
}
