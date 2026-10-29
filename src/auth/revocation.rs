//! jti 黑名单（feature `revocation`）。
//!
//! `docs/11-security-baseline.md` 承诺「登出 / 吊销时把 `jti` 写入 Redis」。
//! 本模块落地该能力：键 `{ns}:j:{jti}`，TTL = 令牌**剩余有效期**（过期后自然淘汰，
//! 无需清理任务）。
//!
//! 与 T1 缓存的区别：这是**安全状态**——走 [`SecureStore`]，TTL 必填、失败不降级。
//! 吊销检查失败时是放行还是拒绝，由 `[auth].revocation.fail_closed` 决定
//! （默认 `false`：可用性优先；资金类系统建议打开）。

use std::time::Duration;

use crate::cache::key::Class;
use crate::cache::{CacheService, SecureStore};
use crate::errors::AppResult;

/// jti 黑名单存储。
#[derive(Clone)]
pub struct RevocationStore {
    store: SecureStore,
    fail_closed: bool,
}

impl std::fmt::Debug for RevocationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevocationStore").finish_non_exhaustive()
    }
}

impl RevocationStore {
    /// 以缓存服务装配（复用 `[cache]` 的连接与命名空间）。
    ///
    /// `fail_closed`：吊销检查失败时是否拒绝请求（`false` = 按未吊销处理）。
    #[must_use]
    pub fn new(cache: CacheService, fail_closed: bool) -> Self {
        Self {
            store: cache.secure(Class::Revocation),
            fail_closed,
        }
    }

    /// 吊销检查失败时是否拒绝请求。
    #[must_use]
    pub fn fail_closed(&self) -> bool {
        self.fail_closed
    }

    /// 吊销一个 `jti`，`ttl` 应为令牌的剩余有效期。
    ///
    /// # Errors
    /// `ttl` 为零返回 `10001`；后端失败返回 `50004`。
    pub async fn revoke(&self, jti: &str, ttl: Duration) -> AppResult<()> {
        if jti.is_empty() {
            return Ok(());
        }
        self.store.put(jti, b"1", ttl).await
    }

    /// 按令牌过期时刻吊销（已过期的令牌无需写入）。
    ///
    /// # Errors
    /// 后端失败返回 `50004`。
    pub async fn revoke_until(&self, jti: &str, expires_at: i64) -> AppResult<()> {
        let now = crate::utils::now_unix_secs();
        let remaining = expires_at.saturating_sub(now);
        if remaining <= 0 {
            return Ok(());
        }
        let ttl = Duration::from_secs(u64::try_from(remaining).unwrap_or(u64::MAX));
        self.revoke(jti, ttl).await
    }

    /// 是否已被吊销。
    ///
    /// # Errors
    /// 后端失败返回 `50004`。
    pub async fn is_revoked(&self, jti: &str) -> AppResult<bool> {
        if jti.is_empty() {
            return Ok(false);
        }
        self.store.exists(jti).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::CacheSection;

    async fn store() -> RevocationStore {
        let config = CacheSection {
            namespace: "test".to_string(),
            ..CacheSection::default()
        };
        RevocationStore::new(CacheService::from_config(&config).await.unwrap(), false)
    }

    #[tokio::test]
    async fn should_report_revoked_after_revoke() {
        let store = store().await;
        assert!(!store.is_revoked("jti-1").await.unwrap());
        store
            .revoke("jti-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(store.is_revoked("jti-1").await.unwrap());
    }

    #[tokio::test]
    async fn should_skip_expired_token() {
        let store = store().await;
        store.revoke_until("jti-2", 0).await.unwrap();
        assert!(!store.is_revoked("jti-2").await.unwrap());
    }

    #[tokio::test]
    async fn should_ignore_empty_jti() {
        let store = store().await;
        store.revoke("", Duration::from_secs(60)).await.unwrap();
        assert!(!store.is_revoked("").await.unwrap());
    }
}
