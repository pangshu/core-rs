//! 队列探针（`/ready`）。

use async_trait::async_trait;

use crate::queue::QueueService;
use crate::telemetry::HealthCheck;

/// 队列探针（`/ready` 使用）。
pub struct QueueHealth {
    queue: QueueService,
    name: String,
}

impl QueueHealth {
    /// 以队列服务创建探针。
    #[must_use]
    pub fn new(queue: QueueService) -> Self {
        Self {
            queue,
            name: "queue".to_string(),
        }
    }
}

#[async_trait]
impl HealthCheck for QueueHealth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check(&self) -> Result<(), String> {
        // 原文只进日志，对外由注册表统一替换成 `queue unavailable`
        self.queue.ping().await.map_err(|error| error.to_string())
    }
}
