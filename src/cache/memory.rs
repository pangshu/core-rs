//! 进程内缓存后端（feature = "cache-memory"，默认）：moka TTL 缓存，
//! 零外部依赖，单机部署与开发/测试的默认值。
//!
//! 计数（`incr`）不走路由 moka——读-改-写跨 await 会丢计数，改为专用
//! `Mutex<HashMap>` 计数表：锁内无 await，原子递增，窗口 TTL 首次计数时固定。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{Cache, CacheError};
use crate::config::sections::CacheMemorySettings;

/// 内存后端条目：值 + 写入时指定的 ttl（供 Expiry 在创建/更新时计算过期）
#[derive(Debug, Clone)]
struct MemEntry {
    value: String,
    ttl: Option<Duration>,
}

/// 计数条目：原子递增的值 + 窗口到期时刻（None = 不过期，PERSIST）
#[derive(Debug, Clone)]
struct Counter {
    value: i64,
    expires_at: Option<Instant>,
}

/// 按「条目自身 ttl，缺省用配置的默认 ttl」计算过期；两者皆无则不过期
struct MemExpiry {
    default_ttl: Option<Duration>,
}

impl moka::Expiry<String, MemEntry> for MemExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        entry: &MemEntry,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        entry.ttl.or(self.default_ttl)
    }

    fn expire_after_update(
        &self,
        _key: &String,
        entry: &MemEntry,
        _updated_at: std::time::Instant,
        _current_duration: Option<Duration>,
    ) -> Option<Duration> {
        entry.ttl.or(self.default_ttl)
    }
}

#[derive(Debug)]
pub struct MemoryCache {
    inner: moka::future::Cache<String, MemEntry>,
    /// incr 专用计数表：锁内无 await 点，读-改-写原子（限流的正确性依赖于此）
    counters: Mutex<HashMap<String, Counter>>,
}

impl MemoryCache {
    pub fn new(settings: &CacheMemorySettings) -> Self {
        let default_ttl = (settings.default_ttl_secs > 0)
            .then(|| Duration::from_secs(settings.default_ttl_secs));
        Self {
            inner: moka::future::Cache::builder()
                .max_capacity(settings.max_capacity.max(1))
                .expire_after(MemExpiry { default_ttl })
                .build(),
            counters: Mutex::new(HashMap::new()),
        }
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, HashMap<String, Counter>> {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 清扫过期计数条目（高基数窗口 key 用完即忘，不清扫则无上界增长）
    fn sweep_expired(map: &mut HashMap<String, Counter>) {
        if map.len() <= 1024 {
            return;
        }
        let now = Instant::now();
        map.retain(|_, c| c.expires_at.map(|e| e > now).unwrap_or(true));
    }

    /// 计数条目的窗口操作；key 不在计数表时返回 None（走 moka 路径）
    fn expire_counter(&self, key: &str, ttl: Option<Duration>) -> Option<bool> {
        let mut map = self.counters();
        let c = map.get_mut(key)?;
        if c.expires_at.map(|e| e <= Instant::now()).unwrap_or(false) {
            return Some(false);
        }
        c.expires_at = match ttl {
            Some(d) if d > Duration::ZERO => Some(Instant::now() + d),
            // PERSIST：None / ZERO 清除窗口
            _ => None,
        };
        Some(true)
    }
}

#[async_trait::async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        // 计数表优先：incr 写入的值对普通 get 可见
        {
            let map = self.counters();
            if let Some(c) = map.get(key) {
                let expired = c.expires_at.map(|e| e <= Instant::now()).unwrap_or(false);
                if !expired {
                    return Ok(Some(c.value.to_string()));
                }
            }
        }
        Ok(self.inner.get(key).await.map(|e| e.value))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError> {
        // 同 key 的计数被显式 set 覆盖：移除计数条目（last write wins）
        self.counters().remove(key);
        self.inner
            .insert(key.to_string(), MemEntry {
                value: value.to_string(),
                ttl,
            })
            .await;
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<(), CacheError> {
        self.counters().remove(key);
        self.inner.invalidate(key).await;
        Ok(())
    }

    /// 原子递增（锁内无 await 点）：首次计数后由调用方 `expire` 设窗口 TTL。
    /// 多进程部署仍需 redis 后端——本原语只保证**进程内**原子。
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError> {
        let mut map = self.counters();
        Self::sweep_expired(&mut map);
        let now = Instant::now();
        let entry = map.entry(key.to_string()).or_insert(Counter {
            value: 0,
            expires_at: None,
        });
        // 已过期的窗口从 0 重新计数
        if entry.expires_at.map(|e| e <= now).unwrap_or(false) {
            entry.value = 0;
            entry.expires_at = None;
        }
        entry.value += delta;
        Ok(entry.value)
    }

    /// 计数条目：Some(ZERO/非正) = PERSIST（清除窗口）；Some(>0) = 设窗口。
    /// moka 条目 TTL 写入时固定：实现为「读出 → 带 TTL 重写」，对原子语义
    /// 敏感的场景请用 redis 后端。
    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError> {
        // 计数表路径在同步函数里完成（锁卫绝不跨 await）
        if let Some(result) = self.expire_counter(key, ttl) {
            return Ok(result);
        }
        let Some(value) = self.get(key).await? else {
            return Ok(false);
        };
        self.set(key, &value, ttl.filter(|d| *d > Duration::ZERO)).await?;
        Ok(true)
    }

    /// 进程内缓存无独立组件，进程在即可用
    async fn ping(&self) -> Result<(), CacheError> {
        Ok(())
    }
}
