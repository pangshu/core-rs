//! 刷新令牌轮换与重用检测（feature `revocation`）。
//!
//! ## 为什么需要它
//!
//! refresh 令牌是**纯无状态 JWT**：签出去之后服务端不再持有任何状态，直到它过期
//! （`[auth].jwt.refresh_ttl`，默认 7 天）。这意味着
//!
//! - 令牌一旦泄漏（设备丢失、XSS 读走 localStorage、进了日志 / 崩溃上报），
//!   攻击者在剩余有效期内可以持续换取 access 令牌；
//! - **合法用户正常刷新不会让泄漏的那条失效**——两条 refresh 并存，服务端
//!   完全无感知，因为没有任何「同一条令牌被用了两次」的信号。
//!
//! 本模块补上两个动作：
//!
//! 1. **轮换**：每次刷新都签发新的 refresh，旧的立即标记「已用」；
//! 2. **重用检测**：同一条 refresh 被第二次使用 → 判定泄漏 → 整条家族拉黑。
//!
//! 判定为泄漏后的结果是「用户和攻击者一起下线」：合法用户重新登录即可，
//! 攻击者失去访问。这是业界通行取舍——**宁可误杀，不可漏放**。
//!
//! ## 键布局
//!
//! 与 jti 黑名单共用 `Class::Revocation` 命名空间（同一套 Redis 连接与隔离前缀），
//! 但键前缀不同，互不相交：
//!
//! - `u:{jti}` —— 该 refresh 已被使用过（TTL = 令牌**剩余**有效期，过期自然淘汰）
//! - `f:{fid}` —— 该家族已整体拉黑（TTL = `refresh_ttl`，家族最长寿命）
//!
//! ## 已知边界
//!
//! - 家族拉黑**不影响已签发的 access 令牌**：它们由 `[auth].jwt.access_ttl`
//!   （默认 15 分钟）兜底。要立刻失效得把它们逐个写进 jti 黑名单，代价是每个
//!   请求一次 Redis 查询，不划算。
//! - 旧令牌（引入 `fid` 之前签发的）没有家族 id，调用方按 `sub` 归族。
//! - 并发刷新同一令牌时，[`RefreshRotation::claim`] 依赖后端的 `SET NX` 原子性，
//!   只有一个请求会拿到 `true`，另一个必然被判为重放——这是**有意的**：同一条
//!   refresh 被两个请求同时用，本身就符合泄漏特征。

use std::time::Duration;

use crate::cache::key::Class;
use crate::cache::{CacheService, SecureStore};
use crate::errors::AppResult;

/// 刷新令牌家族状态存储。
#[derive(Clone)]
pub struct RefreshRotation {
    store: SecureStore,
    fail_closed: bool,
}

impl std::fmt::Debug for RefreshRotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshRotation")
            .field("fail_closed", &self.fail_closed)
            .finish_non_exhaustive()
    }
}

impl RefreshRotation {
    /// 以缓存服务装配（复用 `[cache]` 的连接与命名空间）。
    ///
    /// `fail_closed`：存储不可用时是否**拒绝**刷新（`false` = 放行并告警）。
    /// 与 `[auth].revocation.fail_closed` 同源——两者都是「安全状态读不到」这件事。
    #[must_use]
    pub fn new(cache: CacheService, fail_closed: bool) -> Self {
        Self {
            store: cache.secure(Class::Revocation),
            fail_closed,
        }
    }

    /// 存储不可用时是否拒绝刷新。
    #[must_use]
    pub fn fail_closed(&self) -> bool {
        self.fail_closed
    }

    /// 原子占用一个 `jti`，返回**是否为第一次使用**。
    ///
    /// `ttl` 应为该令牌的剩余有效期（过期后这条记录自然淘汰，无需清理）。
    /// 返回 `false` 表示这条 refresh 之前已经被用过——调用方应判定为泄漏。
    ///
    /// # Errors
    /// `ttl` 非法返回 `10001`；后端失败返回 `50004`。
    pub async fn claim(&self, jti: &str, ttl: Duration) -> AppResult<bool> {
        // 令牌刚好在此刻过期时 remaining 可能为 0：`put_once` 拒绝零 TTL，
        // 这里抬到 1 秒——它只影响这条「已用」记录的存活时长，不影响判定。
        let ttl = ttl.max(Duration::from_secs(1));
        self.store.put_once(&format!("u:{jti}"), b"1", ttl).await
    }

    /// 拉黑整个家族（TTL 应为 `refresh_ttl`）。
    ///
    /// # Errors
    /// `ttl` 非法返回 `10001`；后端失败返回 `50004`。
    pub async fn revoke_family(&self, fid: &str, ttl: Duration) -> AppResult<()> {
        let ttl = ttl.max(Duration::from_secs(1));
        self.store.put(&format!("f:{fid}"), b"1", ttl).await
    }

    /// 家族是否已被拉黑。
    ///
    /// # Errors
    /// 后端失败返回 `50004`。
    pub async fn is_family_revoked(&self, fid: &str) -> AppResult<bool> {
        if fid.is_empty() {
            return Ok(false);
        }
        self.store.exists(&format!("f:{fid}")).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CacheSection;

    async fn rotation() -> RefreshRotation {
        let config = CacheSection {
            namespace: "test".to_string(),
            ..CacheSection::default()
        };
        RefreshRotation::new(CacheService::from_config(&config).await.unwrap(), false)
    }

    #[tokio::test]
    async fn should_report_first_claim_then_replay() {
        let rotation = rotation().await;
        assert!(
            rotation
                .claim("jti-a", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            !rotation
                .claim("jti-a", Duration::from_secs(60))
                .await
                .unwrap(),
            "第二次占用必须返回 false（重放）"
        );
        // 不同 jti 互不影响
        assert!(
            rotation
                .claim("jti-b", Duration::from_secs(60))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn should_mark_and_report_family_revoked() {
        let rotation = rotation().await;
        assert!(!rotation.is_family_revoked("fam-1").await.unwrap());
        rotation
            .revoke_family("fam-1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(rotation.is_family_revoked("fam-1").await.unwrap());
        assert!(!rotation.is_family_revoked("fam-2").await.unwrap());
    }

    #[tokio::test]
    async fn should_treat_empty_family_as_not_revoked() {
        let rotation = rotation().await;
        assert!(!rotation.is_family_revoked("").await.unwrap());
    }
}
