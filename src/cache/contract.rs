//! 缓存契约：string 级原语 [`Cache`] + JSON / 回源便捷方法 [`CacheExt`] + 共享句柄 [`CacheHandle`]。
//!
//! 业务只依赖 trait，**换后端不改一行业务代码**。

use std::time::Duration; // 引入时长类型，用于 TTL 参数

use serde::de::DeserializeOwned; // 引入反序列化 trait，get_json 需要
use serde::Serialize; // 引入序列化 trait，set_json 需要

use super::CacheError; // 引入缓存统一错误类型

/// 缓存契约（string 级原语，保持对象安全；JSON / 回源便捷方法见 [`CacheExt`]）。
#[async_trait::async_trait] // 启用 async_trait，使 trait 可含 async 方法且对象安全
pub trait Cache: Send + Sync { // 缓存后端统一契约，要求线程安全
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>; // 按键读取字符串值，未命中为 None
    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<(), CacheError>; // 写入键值并可选设置 TTL
    async fn del(&self, key: &str) -> Result<(), CacheError>; // 删除指定键
    /// 原子计数：key 不存在时从 0 起算（等价 redis `INCRBY`），新建计数器无 TTL。
    /// memory 后端进程内正确，进程间无共享——限流/分布式计数请配 redis 后端。
    async fn incr(&self, key: &str, delta: i64) -> Result<i64, CacheError>; // 对计数键原子累加并返回新值
    /// 重设 key 的 TTL：`None`/`Duration::ZERO` 表示永不过期（等价 `PERSIST`）；
    /// key 不存在返回 `Ok(false)`。
    async fn expire(&self, key: &str, ttl: Option<Duration>) -> Result<bool, CacheError>; // 重设 TTL，返回是否实际命中键
    /// 存活探测（/ready 用）。memory 后端恒 Ok。
    async fn ping(&self) -> Result<(), CacheError>; // 探活后端，供就绪检查聚合
}

/// 便捷方法（JSON 读写 / cache-aside 一站式回源），对 `dyn Cache` 同样可用。
#[allow(async_fn_in_trait)] // 抑制 async fn in trait 的 lint（本 trait 不做对象安全要求）
pub trait CacheExt: Cache { // 在 Cache 之上扩展 JSON 与回源便捷方法
    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, CacheError> { // 读取并反序列化为 T，未命中为 None
        let Some(s) = self.get(key).await? else { // 先取原始字符串，未命中则走 else 分支
            return Ok(None); // 缓存未命中直接返回 None
        };
        match serde_json::from_str(&s) { // 尝试把字符串反序列化为目标类型
            Ok(v) => Ok(Some(v)), // 反序列化成功，返回 Some
            // 脏数据（如缓存结构变更）按 miss 处理让业务回源，而不是打挂接口
            Err(e) => { // 反序列化失败时的降级分支
                tracing::warn!(key = %key, error = %e, "cache value corrupt, treat as miss"); // 记录告警日志
                Ok(None) // 按未命中返回，交由业务回源
            }
        }
    }

    async fn set_json<T: Serialize + Sync>( // 把值序列化为 JSON 后写入缓存
        &self, // 方法接收者：缓存实例引用
        key: &str, // 缓存键
        value: &T, // 待序列化的值
        ttl: Option<Duration>, // 可选 TTL，None 表示不过期
    ) -> Result<(), CacheError> { // 返回单元结果
        self.set(key, &serde_json::to_string(value)?, ttl).await // 序列化并写入，返回写入结果
    }

    /// cache-aside 一站式读取：先查缓存，miss（**或缓存故障**）时用 `load` 回源并回填。
    /// 缓存读写失败只记日志、按 miss 处理，**不会传染成业务 500**；
    /// `load` 的错误类型 `E` 原样透传（通常是 `AppError`）。
    async fn get_or_load<T, E, F, Fut>(&self, key: &str, ttl: Option<Duration>, load: F) -> Result<T, E> // 先查缓存，miss 则调用 load 回源并回填
    where // 泛型约束子句开始
        T: Serialize + DeserializeOwned + Send + Sync, // 缓存值需可序列化/反序列化且线程安全
        E: From<CacheError>, // 业务错误需能由缓存错误转换而来
        F: FnOnce() -> Fut + Send, // 回源闭包只调用一次且可跨线程
        Fut: std::future::Future<Output = Result<T, E>> + Send, // 回源返回的未来类型约束
    {
        // 读缓存：任何缓存故障按 miss 降级，不传染
        match self.get_json::<T>(key).await { // 尝试从缓存读取
            Ok(Some(v)) => return Ok(v), // 命中直接返回
            Ok(None) => {} // 未命中则继续往下回源
            Err(e) => { // 缓存读故障时降级
                tracing::warn!(key = %key, error = %e, "cache read failed, degrade to direct load") // 记录告警但不中断
            }
        }
        let value = load().await?; // 调用回源闭包取真实数据，错误原样透传
        if let Err(e) = self.set_json(key, &value, ttl).await { // 尝试回填缓存
            tracing::warn!(key = %key, error = %e, "cache write failed after load"); // 回填失败仅记日志，不影响返回
        }
        Ok(value) // 返回回源得到的值
    }
}

impl<T: Cache + ?Sized> CacheExt for T {} // 为所有 Cache 实现（含 dyn）自动实现 CacheExt

/// 共享缓存句柄（存于 CoreState）
pub type CacheHandle = std::sync::Arc<dyn Cache>; // 类型别名：线程安全的缓存 trait 对象句柄
