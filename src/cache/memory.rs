//! 进程内缓存后端（`cache-inmem`，基于 moka）。
//!
//! 单实例默认后端；多实例需切 `cache-redis`，否则各实例缓存不一致
//! （见 `docs/02` §12）。
//!
//! 容量按**字节**计权（`key.len() + value.len()`），不是条目数——否则
//! `10 万条目 × 单值 1 MiB` 会变成 100 GiB 的内存放大面。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use moka::Expiry;
use moka::sync::Cache as MokaCache;

use crate::cache::Cache;
use crate::errors::{AppError, AppResult};

/// 缓存条目：值 + 该条目的 TTL。
type Entry = (Arc<Vec<u8>>, Duration);

/// 按条目 TTL 过期的策略（moka 的全局 TTL 无法表达「每个 key 不同 TTL」）。
struct EntryExpiry;

impl Expiry<String, Entry> for EntryExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Entry,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(value.1)
    }

    /// **覆盖写必须按新 TTL 重新计时**。
    ///
    /// moka 的默认实现返回 `duration_until_expiry`（保留原到期时刻），会让
    /// 「用短 TTL 覆盖来撤销敏感数据」静默失效，也与 Redis `SET ... PX` 的语义相反。
    fn expire_after_update(
        &self,
        _key: &String,
        value: &Entry,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(value.1)
    }
}

/// 进程内缓存（moka）。
///
/// `max_capacity` 的单位是**字节**（按 `key + value` 计权）。
#[derive(Clone)]
pub struct MemoryCache {
    store: MokaCache<String, Entry>,
    /// `set_nx` / `del_if_eq` 的临界区：moka 没有原子的「判存在 + 插入」。
    gate: Arc<Mutex<()>>,
}

impl MemoryCache {
    /// 以最大字节数创建。
    #[must_use]
    pub fn new(max_capacity: u64) -> Self {
        let store = MokaCache::builder()
            .max_capacity(max_capacity)
            .weigher(|key: &String, value: &Entry| -> u32 {
                let bytes = key.len().saturating_add(value.0.len());
                u32::try_from(bytes).unwrap_or(u32::MAX)
            })
            .expire_after(EntryExpiry)
            .build();
        Self {
            store,
            gate: Arc::new(Mutex::new(())),
        }
    }

    /// 当前条目数（近似值，含尚未被 moka 清理的过期项）。
    #[must_use]
    pub fn len(&self) -> u64 {
        self.store.entry_count()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取得临界区锁；中毒时用 `into_inner()` 恢复（临界区内不会 panic）。
    fn gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new(64 * 1024 * 1024)
    }
}

impl std::fmt::Debug for MemoryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCache")
            .field("entries", &self.store.entry_count())
            .finish()
    }
}

/// 校验 TTL：必须为正。`None` 与零一律拒绝（永久键）。
fn require_ttl(ttl: Option<Duration>) -> AppResult<Duration> {
    match ttl {
        Some(ttl) if !ttl.is_zero() => Ok(ttl),
        _ => Err(AppError::invalid_param("缓存 TTL 必须大于 0")),
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        Ok(self
            .store
            .get(key)
            .map(|(bytes, _ttl)| bytes.as_ref().clone()))
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> AppResult<()> {
        let ttl = require_ttl(ttl)?;
        self.store
            .insert(key.to_string(), (Arc::new(value.to_vec()), ttl));
        Ok(())
    }

    async fn del(&self, key: &str) -> AppResult<()> {
        self.store.invalidate(key);
        Ok(())
    }

    async fn take(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        // moka 的 `remove` 是原子的：返回被移除的值
        Ok(self
            .store
            .remove(key)
            .map(|(bytes, _ttl)| bytes.as_ref().clone()))
    }

    async fn set_nx(&self, key: &str, value: &[u8], ttl: Duration) -> AppResult<bool> {
        let ttl = require_ttl(Some(ttl))?;
        let _guard = self.gate();
        if self.store.get(key).is_some() {
            return Ok(false);
        }
        self.store
            .insert(key.to_string(), (Arc::new(value.to_vec()), ttl));
        Ok(true)
    }

    async fn del_if_eq(&self, key: &str, expected: &[u8]) -> AppResult<bool> {
        let _guard = self.gate();
        let Some((current, _ttl)) = self.store.get(key) else {
            return Ok(false);
        };
        if current.as_slice() != expected {
            return Ok(false);
        }
        self.store.invalidate(key);
        Ok(true)
    }

    async fn exists(&self, key: &str) -> AppResult<bool> {
        Ok(self.store.get(key).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Option<Duration> = Some(Duration::from_secs(60));

    #[tokio::test]
    async fn should_roundtrip_value() {
        let cache = MemoryCache::default();
        cache.set("k", b"v", TTL).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[tokio::test]
    async fn should_return_none_when_missing() {
        let cache = MemoryCache::default();
        assert!(cache.get("absent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_delete_value() {
        let cache = MemoryCache::default();
        cache.set("k", b"v", TTL).await.unwrap();
        cache.del("k").await.unwrap();
        assert!(cache.get("k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn should_expire_after_ttl() {
        let cache = MemoryCache::default();
        cache
            .set("k", b"v", Some(Duration::from_millis(30)))
            .await
            .unwrap();
        assert!(cache.get("k").await.unwrap().is_some());

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            cache.get("k").await.unwrap().is_none(),
            "超过条目 TTL 后应视为未命中"
        );
    }

    #[tokio::test]
    async fn should_reject_zero_or_missing_ttl() {
        let cache = MemoryCache::default();
        assert!(cache.set("k", b"v", None).await.is_err());
        assert!(cache.set("k", b"v", Some(Duration::ZERO)).await.is_err());
    }

    #[tokio::test]
    async fn should_refresh_ttl_on_update() {
        let cache = MemoryCache::default();
        cache
            .set("k", b"v1", Some(Duration::from_millis(200)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // 覆盖写：按新 TTL 重新计时
        cache
            .set("k", b"v2", Some(Duration::from_millis(200)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            cache.get("k").await.unwrap().is_some(),
            "覆盖写应刷新到期时间（当前实现若沿用原到期时刻，此处会失败）"
        );
    }

    #[tokio::test]
    async fn should_shorten_ttl_on_update() {
        let cache = MemoryCache::default();
        cache
            .set("k", b"v1", Some(Duration::from_secs(30)))
            .await
            .unwrap();
        cache
            .set("k", b"v2", Some(Duration::from_millis(30)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            cache.get("k").await.unwrap().is_none(),
            "缩短 TTL 的覆盖写应生效"
        );
    }

    #[tokio::test]
    async fn should_take_only_once() {
        let cache = MemoryCache::default();
        cache.set("otp", b"123456", TTL).await.unwrap();
        assert_eq!(
            cache.take("otp").await.unwrap().as_deref(),
            Some(&b"123456"[..])
        );
        assert!(
            cache.take("otp").await.unwrap().is_none(),
            "第二次应已不存在"
        );
    }

    #[tokio::test]
    async fn should_set_only_when_absent() {
        let cache = MemoryCache::default();
        assert!(
            cache
                .set_nx("k", b"a", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            !cache
                .set_nx("k", b"b", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some(&b"a"[..]));
    }

    #[tokio::test]
    async fn should_delete_only_when_value_matches() {
        let cache = MemoryCache::default();
        cache.set("k", b"mine", TTL).await.unwrap();
        assert!(!cache.del_if_eq("k", b"other").await.unwrap());
        assert!(cache.get("k").await.unwrap().is_some());
        assert!(cache.del_if_eq("k", b"mine").await.unwrap());
        assert!(cache.get("k").await.unwrap().is_none());
    }
}
