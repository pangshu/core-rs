//! 生产句柄。

use crate::errors::AppResult;
use crate::queue::service::validate_queue_name;
use crate::queue::{Message, QueueService};

/// 队列生产句柄（`state.queue.producer("orders")`）。
#[derive(Debug, Clone)]
pub struct Producer {
    service: QueueService,
    queue: String,
}

impl Producer {
    pub(crate) fn new(service: QueueService, queue: String) -> Self {
        Self { service, queue }
    }

    /// 目标队列名。
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// 投递一条消息，返回消息 ID（幂等键）。
    ///
    /// # Errors
    /// 队列名非法返回 `10001`；消息体超过 `[queue].max_payload_bytes` 返回 `10002`；
    /// 后端不可用时返回 `50003`（`UPSTREAM_ERROR`）。
    pub async fn publish(&self, payload: impl Into<Vec<u8>>) -> AppResult<String> {
        validate_queue_name(&self.queue)?;
        let payload = payload.into();
        self.service.check_payload_size(payload.len())?;

        let message = Message::new(self.queue.clone(), payload);
        let id = message.id.clone();
        self.service.backend().enqueue(&self.queue, message).await?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::QueueSection;
    use crate::errors::ErrorCode;
    use crate::queue::QueueService;

    #[tokio::test]
    async fn should_publish_and_report_queue_depth() {
        let service = QueueService::from_config(&QueueSection::default())
            .await
            .unwrap();
        let producer = service.producer("orders");

        let id = producer.publish(b"hello".to_vec()).await.unwrap();
        assert!(!id.is_empty());
        assert_eq!(producer.queue(), "orders");
        assert_eq!(service.len("orders").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn should_reject_oversized_payload() {
        let config = QueueSection {
            max_payload_bytes: 4,
            ..QueueSection::default()
        };
        let service = QueueService::from_config(&config).await.unwrap();

        let error = service
            .producer("orders")
            .publish(b"hello".to_vec())
            .await
            .expect_err("超过 max_payload_bytes 应拒绝");
        assert_eq!(error.code(), ErrorCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            service.len("orders").await.unwrap(),
            0,
            "被拒绝的消息不应入队"
        );
    }

    #[tokio::test]
    async fn should_reject_illegal_queue_name() {
        let service = QueueService::from_config(&QueueSection::default())
            .await
            .unwrap();

        let error = service
            .producer("orders:ready")
            .publish(b"x".to_vec())
            .await
            .expect_err("队列名含冒号应拒绝");
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
    }
}
