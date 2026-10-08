//! 缓存统一错误 [`CacheError`]。

/// 缓存操作错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 与 thiserror 错误实现
pub enum CacheError { // 缓存统一错误枚举
    #[error("create redis pool failed: {0}")] // 错误文案：创建 Redis 连接池失败
    #[cfg(feature = "cache-redis")] // 仅在 Redis feature 下存在该变体
    Create(#[from] deadpool_redis::CreatePoolError), // 包装连接池创建错误并自动转换
    #[error("redis pool error: {0}")] // 错误文案：Redis 连接池取连接失败
    #[cfg(feature = "cache-redis")] // 仅在 Redis feature 下存在该变体
    Pool(#[from] deadpool_redis::PoolError), // 包装连接池运行时错误
    #[error("redis error: {0}")] // 错误文案：Redis 命令执行错误
    #[cfg(feature = "cache-redis")] // 仅在 Redis feature 下存在该变体
    Redis(#[from] deadpool_redis::redis::RedisError), // 包装底层 Redis 错误
    #[error("cache serialization error: {0}")] // 错误文案：JSON 序列化/反序列化失败
    Serde(#[from] serde_json::Error), // 包装 serde_json 错误
    #[error("cache backend `{0}` requires a feature (cache-memory / cache-redis) that is not enabled")] // 错误文案：后端所需 feature 未开启
    FeatureDisabled(String), // 记录所需后端名，提示未编译对应 feature
    #[error("cache backend `{0}` is misconfigured: {1}")] // 错误文案：后端配置有误
    Config(String, String), // 记录后端名与具体配置问题
    #[error("unknown cache backend: {0} (expected memory / redis)")] // 错误文案：未知后端名
    UnknownBackend(String), // 记录无法识别的后端名
}
