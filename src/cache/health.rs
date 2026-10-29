//! 缓存探针（`/ready`）。

use async_trait::async_trait;

use crate::cache::CacheService;
use crate::telemetry::HealthCheck;

/// 缓存探针（`/ready` 使用，见 `docs/07` §3.24）。
pub struct CacheHealth {
    cache: CacheService,
    name: String,
}

impl CacheHealth {
    /// 以缓存服务创建探针。
    #[must_use]
    pub fn new(cache: CacheService) -> Self {
        Self {
            cache,
            name: "cache".to_string(),
        }
    }
}

#[async_trait]
impl HealthCheck for CacheHealth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check(&self) -> Result<(), String> {
        // 原文只进日志，对外由注册表统一替换成 `cache unavailable`
        self.cache.ping().await.map_err(|error| error.to_string())
    }
}
