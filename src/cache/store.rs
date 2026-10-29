//! 语义承载类存储（T2）门面。
//!
//! `CacheService` 是 T1（透明加速）：可丢、可重建、后端异常时降级。
//! 幂等、消费去重、jti 黑名单、验证码这些数据**丢了会改变业务或安全结果**，
//! 因此走 [`SecureStore`]：
//!
//! - **TTL 必填**（`Duration`，不是 `Option`）——签名上杜绝「忘给 TTL → 永久键」；
//! - **值有上限**（`[cache].limits.max_value_bytes`）；
//! - **失败不降级**——后端错误原样返回，由调用方决定 fail-open / fail-closed；
//! - **键按 class 隔离**——`i:` / `q:` / `j:` / `o:` 与业务值 `c:` 永不相交。
//!
//! 原子原语（[`take`](Self::take) / [`put_once`](Self::put_once) /
//! [`del_if_eq`](Self::del_if_eq)）由后端提供，不做 `get` + `del` 的两步拼装。

use std::time::Duration;

use crate::cache::key::Class;
use crate::cache::service::CacheService;
use crate::errors::{AppError, AppResult};

/// 语义承载类存储（T2）。
#[derive(Clone)]
pub struct SecureStore {
    cache: CacheService,
    class: Class,
}

impl std::fmt::Debug for SecureStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureStore")
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl SecureStore {
    pub(crate) fn new(cache: CacheService, class: Class) -> Self {
        Self { cache, class }
    }

    /// 当前存储的键分类。
    #[must_use]
    pub fn class(&self) -> Class {
        self.class
    }

    fn key(&self, rest: &str) -> AppResult<String> {
        self.cache.class_key(self.class, rest)
    }

    /// 读取（不降级：后端错误原样返回）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端失败返回 `50004`。
    pub async fn get(&self, rest: &str) -> AppResult<Option<Vec<u8>>> {
        let key = self.key(rest)?;
        self.cache.backend().get(&key).await
    }

    /// 写入（TTL 必填且为正）。
    ///
    /// # Errors
    /// 键非法 / TTL 为零返回 `10001`；值超限返回 `10002`；后端失败返回 `50004`。
    pub async fn put(&self, rest: &str, value: &[u8], ttl: Duration) -> AppResult<()> {
        let key = self.key(rest)?;
        require_ttl(ttl)?;
        self.cache.check_value_size(value.len())?;
        self.cache.backend().set(&key, value, Some(ttl)).await
    }

    /// 原子读取并删除（一次性消费）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端失败返回 `50004`。
    pub async fn take(&self, rest: &str) -> AppResult<Option<Vec<u8>>> {
        let key = self.key(rest)?;
        self.cache.backend().take(&key).await
    }

    /// 原子「仅当不存在时写入」；返回是否写入成功。
    ///
    /// # Errors
    /// 键非法 / TTL 为零返回 `10001`；值超限返回 `10002`；后端失败返回 `50004`。
    pub async fn put_once(&self, rest: &str, value: &[u8], ttl: Duration) -> AppResult<bool> {
        let key = self.key(rest)?;
        require_ttl(ttl)?;
        self.cache.check_value_size(value.len())?;
        self.cache.backend().set_nx(&key, value, ttl).await
    }

    /// 原子「仅当值等于 `expected` 时删除」；返回是否删除。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端失败返回 `50004`。
    pub async fn del_if_eq(&self, rest: &str, expected: &[u8]) -> AppResult<bool> {
        let key = self.key(rest)?;
        self.cache.backend().del_if_eq(&key, expected).await
    }

    /// 删除（不存在视为成功）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端失败返回 `50004`。
    pub async fn del(&self, rest: &str) -> AppResult<()> {
        let key = self.key(rest)?;
        self.cache.backend().del(&key).await
    }

    /// 是否存在（不传输值本身）。
    ///
    /// # Errors
    /// 键非法返回 `10001`；后端失败返回 `50004`。
    pub async fn exists(&self, rest: &str) -> AppResult<bool> {
        let key = self.key(rest)?;
        self.cache.backend().exists(&key).await
    }
}

fn require_ttl(ttl: Duration) -> AppResult<()> {
    if ttl.is_zero() {
        return Err(AppError::invalid_param("语义承载类缓存必须带正 TTL"));
    }
    Ok(())
}
