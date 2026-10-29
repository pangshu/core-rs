//! 缓存后端抽象。
//!
//! 键由 [`CacheService`](crate::cache::CacheService) 统一加命名空间后传入，
//! 后端不感知命名空间。
//!
//! ## 为什么有 `take` / `set_nx` / `del_if_eq`
//!
//! T1（透明加速）只需要 `get` / `set` / `del`；但 T2（语义承载：幂等、消费去重、
//! 验证码、锁）需要**原子**原语——`get` + `del` 两步之间存在窗口，会让同一个验证码
//! 被并发消费两次。这三个方法**没有默认实现**，避免第三方后端静默退化成非原子实现。

use std::time::Duration;

use async_trait::async_trait;

use crate::errors::AppResult;

/// 缓存后端抽象（object-safe）。
///
/// 键由 [`CacheService`](crate::cache::CacheService) 统一加命名空间后传入，后端不感知命名空间。
///
/// **TTL 约束**：`set` / `set_nx` 的 `ttl` 必须为正；`None` 与 `Duration::ZERO`
/// 一律返回 `10001`——永久键会让敏感数据（会话、幂等记录、验证码）永不淘汰。
#[async_trait]
pub trait Cache: Send + Sync + 'static {
    /// 读取；未命中返回 `Ok(None)`。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`（`CACHE_ERROR`）。
    async fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>>;

    /// 写入；`ttl` 必须为正（`None` / 零返回 `10001`）。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`；`ttl` 非法时返回 `10001`。
    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> AppResult<()>;

    /// 删除（不存在视为成功）。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`。
    async fn del(&self, key: &str) -> AppResult<()>;

    /// **原子**读取并删除（`GETDEL` / Lua）。
    ///
    /// 用于「只能消费一次」的数据（验证码、一次性令牌）。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`。
    async fn take(&self, key: &str) -> AppResult<Option<Vec<u8>>>;

    /// **原子**「仅当键不存在时写入」（`SET NX PX`）。返回 `true` 表示写入成功。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`；`ttl` 非法时返回 `10001`。
    async fn set_nx(&self, key: &str, value: &[u8], ttl: Duration) -> AppResult<bool>;

    /// **原子**「仅当当前值等于 `expected` 时删除」（compare-and-del）。返回是否删除。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`。
    async fn del_if_eq(&self, key: &str, expected: &[u8]) -> AppResult<bool>;

    /// 是否存在（不传输值本身）。
    ///
    /// # Errors
    /// 后端访问失败时返回 `50004`。
    async fn exists(&self, key: &str) -> AppResult<bool> {
        Ok(self.get(key).await?.is_some())
    }

    /// 连通性探测（供 `/ready` 使用）；无外部依赖的后端默认成功。
    ///
    /// # Errors
    /// 后端不可用时返回 `50004`。
    async fn ping(&self) -> AppResult<()> {
        Ok(())
    }
}
