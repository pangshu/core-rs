//! 消费句柄。
//!
//! `recv` → 业务处理 → `ack`（成功）/ `nack`（失败）。`nack` 按 `[queue].retry`
//! 退避重投，耗尽后入死信（`[queue].dlq`）。
//!
//! **推荐顺序：先落库再 ack**；`[queue].idempotency.enabled` 开启时，`recv` 会写
//! `processing` 占位（短租约），`ack` 成功后改写为 `done`（长保留）—— 因此
//! **`ack` 不只是「告诉后端已消费」，也是幂等窗口的提交点**，业务成功后必须调用。
//!
//! 崩溃语义：占位是 `processing` + 租约，进程在处理中途死掉时，租约到期后重投的
//! 消息可以重新占位并再处理一次（at-least-once），不会被当成「已消费」永久跳过。

use std::sync::Arc;

use crate::errors::AppResult;
use crate::queue::dlq::Decision;
use crate::queue::service::{DedupClaim, validate_queue_name};
use crate::queue::{IDLE_POLL_INTERVAL, Message, QueueService};

/// 队列消费句柄（`state.queue.consumer("orders")`）。
#[derive(Debug, Clone)]
pub struct Consumer {
    service: QueueService,
    queue: String,
}

impl Consumer {
    pub(crate) fn new(service: QueueService, queue: String) -> Self {
        Self { service, queue }
    }

    /// 源队列名。
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// 取一条消息；队列为空返回 `Ok(None)`。
    ///
    /// 开启幂等去重时，命中已消费 / 正在被消费的 `message_id` 会自动 `ack` 并继续取
    /// 下一条。
    ///
    /// # Errors
    /// 队列名非法返回 `10001`；后端不可用时返回 `50003`；消息体超过
    /// `[queue].max_payload_bytes` 时返回 `10002`（该消息不会被交给业务）。
    pub async fn recv(&self) -> AppResult<Option<Message>> {
        validate_queue_name(&self.queue)?;

        loop {
            let Some(message) = self.service.backend().dequeue(&self.queue).await? else {
                return Ok(None);
            };

            // 出站校验：只拦入队挡不住「别的生产者 / 老版本 / 手工灌入」的消息。
            // 超限消息不交给业务，也不重投（重投只会无限循环），直接入死信由人工处理。
            if let Err(error) = self.service.check_payload_size(message.payload.len()) {
                tracing::error!(
                    %error,
                    queue = %self.queue,
                    id = %message.id,
                    "消息体超限，转入死信"
                );
                self.service
                    .backend()
                    .dead_letter(&self.queue, message)
                    .await?;
                continue;
            }

            if self.service.dedup_enabled() {
                match self.service.dedup_claim(&self.queue, &message.id).await {
                    Ok(DedupClaim::First) => {}
                    Ok(DedupClaim::Duplicate) => {
                        // 已处理完成，或正被另一个消费者处理：确认本条投递，不再交给业务。
                        tracing::debug!(queue = %self.queue, id = %message.id, "命中消费去重键，跳过重复消息");
                        self.service.backend().ack(&self.queue, &message.id).await?;
                        continue;
                    }
                    Err(error) => {
                        if self.service.dedup_fail_closed() {
                            // fail-closed：宁可重投，也不在「无法判重」时执行业务
                            tracing::error!(
                                %error,
                                queue = %self.queue,
                                id = %message.id,
                                "去重占位失败，fail-closed：按失败重投"
                            );
                            self.nack(message).await?;
                            return Err(error);
                        }
                        tracing::warn!(
                            %error,
                            queue = %self.queue,
                            id = %message.id,
                            "去重占位失败，本次降级为不去重"
                        );
                    }
                }
            }

            return Ok(Some(message));
        }
    }

    /// 取一条消息，队列为空时等待 `timeout` 后再返回 `Ok(None)`。
    ///
    /// 等待期间按 `IDLE_POLL_INTERVAL` 轮询：消息在等待窗口内到达时可以立刻返回，
    /// 而不是傻等到 deadline 才看一眼。`timeout = None` 表示只轮询一次。
    ///
    /// # Errors
    /// 后端不可用时返回 `50003`。
    pub async fn recv_with_wait(
        &self,
        timeout: Option<std::time::Duration>,
    ) -> AppResult<Option<Message>> {
        if let Some(message) = self.recv().await? {
            return Ok(Some(message));
        }

        let Some(timeout) = timeout.filter(|timeout| !timeout.is_zero()) else {
            return Ok(None);
        };

        // 之前这里写成 `sleep(timeout.min(IDLE_POLL_INTERVAL))`：无论调用方要等多久，
        // 实际都只等 50ms 就返回 None，外层消费循环于是变成 20Hz 空转。
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            tokio::time::sleep(remaining.min(IDLE_POLL_INTERVAL)).await;
            if let Some(message) = self.recv().await? {
                return Ok(Some(message));
            }
        }
    }

    /// 确认消费成功。
    ///
    /// 顺序是**先后端 ack、再把去重占位提升为 `done`**：
    /// - 后端 ack 失败 → 直接返回错误，占位仍是 `processing`，租约到期后会被重投重做；
    /// - 后端 ack 成功但 `done` 写失败 → 只告警，最坏结果是租约到期后**再处理一次**
    ///   （at-least-once），而不是「已确认却被当成新消息重放」。
    ///
    /// # Errors
    /// 消息不属于本消费者的队列返回 `10001`；后端不可用时返回 `50003`。
    pub async fn ack(&self, message: &Message) -> AppResult<()> {
        self.ensure_same_queue(message)?;
        self.service.backend().ack(&self.queue, &message.id).await?;
        self.service.dedup_complete(&self.queue, &message.id).await;
        Ok(())
    }

    /// 消费失败：按策略**退避重投**或**入死信**。
    ///
    /// 重投前会**释放去重占位**，否则重投的消息会被当成「已消费」而跳过。
    ///
    /// 退避期间**不会阻塞调用方**：等待 `delay` 后由后台任务执行 `requeue`。
    /// 在消费循环里 `sleep` 会让一条毒消息把整条队列堵住（`max_attempts` 条毒消息
    /// 就是 `max_attempts × delay` 的停顿）。进程在退避窗口内退出也不会丢消息——
    /// 它仍在后端的 in-flight / 未提交位点里，会被重投或超时回收。
    ///
    /// # Errors
    /// 消息不属于本消费者的队列返回 `10001`；后端不可用时返回 `50003`。
    pub async fn nack(&self, mut message: Message) -> AppResult<()> {
        self.ensure_same_queue(&message)?;
        message.attempts = message.attempts.saturating_add(1);

        if self.service.dedup_enabled() {
            self.service.dedup_release(&self.queue, &message.id).await;
        }

        match self.service.policy().decide(message.attempts) {
            Decision::Retry(delay) => {
                tracing::debug!(
                    queue = %self.queue,
                    id = %message.id,
                    attempts = message.attempts,
                    delay_ms = delay.as_millis() as u64,
                    "消费失败，退避后重投"
                );

                let backend = Arc::clone(self.service.backend());
                let queue = self.queue.clone();
                if delay.is_zero() {
                    return backend.requeue(&queue, message).await;
                }
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if let Err(error) = backend.requeue(&queue, message).await {
                        tracing::error!(
                            %error,
                            queue,
                            "退避重投失败（消息仍在后端 in-flight，会被重投或超时回收）"
                        );
                    }
                });
                Ok(())
            }
            Decision::DeadLetter => {
                tracing::warn!(
                    queue = %self.queue,
                    id = %message.id,
                    attempts = message.attempts,
                    "重试耗尽，转入死信队列"
                );
                self.service
                    .backend()
                    .dead_letter(&self.queue, message)
                    .await
            }
            Decision::Drop => {
                tracing::warn!(
                    queue = %self.queue,
                    id = %message.id,
                    attempts = message.attempts,
                    "重试耗尽且未启用死信队列，消息丢弃"
                );
                self.service.backend().ack(&self.queue, &message.id).await
            }
        }
    }

    /// 校验消息确实来自本消费者的队列。
    ///
    /// 后端 `ack(queue, id)` 是按 `(队列, id)` 定位的：拿 A 队列的消息去 `ack` B 消费者，
    /// 在 Redis 上会 `LREM` 错列表 —— 返回成功，但 A 的消息永远卡在 in-flight。
    /// 这类误用在类型上完全合法，只能运行期拦。
    fn ensure_same_queue(&self, message: &Message) -> AppResult<()> {
        if !message.queue.is_empty() && message.queue != self.queue {
            return Err(crate::errors::AppError::invalid_param(format!(
                "消息属于队列 {:?}，不能用队列 {:?} 的消费者处理",
                message.queue, self.queue
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::config::QueueSection;
    use crate::queue::Message;
    use crate::queue::QueueService;

    async fn service(max_attempts: u32, dlq: bool) -> QueueService {
        let mut config = QueueSection::default();
        config.retry.max_attempts = max_attempts;
        config.dlq.enabled = dlq;
        QueueService::from_config(&config).await.unwrap()
    }

    #[tokio::test]
    async fn should_receive_and_ack() {
        let service = service(3, true).await;
        let producer = service.producer("orders");
        let consumer = service.consumer("orders");

        producer.publish(b"payload".to_vec()).await.unwrap();
        let message = consumer.recv().await.unwrap().expect("应有消息");
        assert_eq!(message.payload, b"payload");

        consumer.ack(&message).await.unwrap();
        assert!(consumer.recv().await.unwrap().is_none(), "ack 后不应重投");
    }

    #[tokio::test]
    async fn should_redeliver_after_nack_before_exhaustion() {
        let service = service(3, true).await;
        let producer = service.producer("orders");
        let consumer = service.consumer("orders");

        producer.publish(b"p".to_vec()).await.unwrap();
        let message = consumer.recv().await.unwrap().expect("应有消息");
        consumer.nack(message).await.unwrap();

        // 退避重投由后台任务在 `delay`（指数退避首档 100ms）后执行，
        // `nack` 本身不阻塞消费循环。
        tokio::time::sleep(Duration::from_millis(250)).await;

        let again = consumer.recv().await.unwrap().expect("应重投");
        assert_eq!(again.attempts, 1);
    }

    #[tokio::test]
    async fn should_move_to_dlq_after_retries_exhausted() {
        let service = service(1, true).await;
        let producer = service.producer("orders");
        let consumer = service.consumer("orders");

        producer.publish(b"p".to_vec()).await.unwrap();
        let message = consumer.recv().await.unwrap().expect("应有消息");
        consumer.nack(message).await.unwrap();

        assert!(consumer.recv().await.unwrap().is_none(), "不应重投");
        let dead = service.dead_letters("orders").await.unwrap();
        assert_eq!(dead.len(), 1, "应进入死信队列");

        assert_eq!(service.replay_dead("orders", 10).await.unwrap(), 1);
        let replayed = consumer.recv().await.unwrap().expect("重放后应可消费");
        assert_eq!(replayed.attempts, 0);
    }

    #[tokio::test]
    async fn should_drop_when_dlq_disabled() {
        let service = service(1, false).await;
        let producer = service.producer("orders");
        let consumer = service.consumer("orders");

        producer.publish(b"p".to_vec()).await.unwrap();
        let message = consumer.recv().await.unwrap().expect("应有消息");
        consumer.nack(message).await.unwrap();

        assert!(consumer.recv().await.unwrap().is_none());
        assert!(service.dead_letters("orders").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_return_none_when_empty() {
        let service = service(3, true).await;
        let consumer = service.consumer("orders");
        assert!(consumer.recv().await.unwrap().is_none());
        assert!(
            consumer
                .recv_with_wait(Some(Duration::from_millis(10)))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// `recv_with_wait` 之前把 `timeout` 压成 `min(50ms)`，调用方要等 800ms 也只看一眼。
    #[tokio::test]
    async fn should_pick_up_message_arriving_during_wait() {
        let service = service(3, true).await;
        let producer = service.producer("orders");
        let consumer = service.consumer("orders");

        let publisher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            producer.publish(b"late".to_vec()).await.unwrap();
        });

        let message = consumer
            .recv_with_wait(Some(Duration::from_millis(800)))
            .await
            .unwrap();
        publisher.await.unwrap();

        assert!(
            message.is_some(),
            "等待窗口内到达的消息应被取到，而不是干等到 deadline"
        );
    }

    /// 出站校验：绕开 `Producer::publish` 直接塞进后端的超大消息不能交给业务。
    #[tokio::test]
    async fn should_dead_letter_oversized_message_on_recv() {
        let config = QueueSection {
            max_payload_bytes: 8,
            ..QueueSection::default()
        };
        let service = QueueService::from_config(&config).await.unwrap();
        let consumer = service.consumer("orders");

        service
            .backend()
            .enqueue("orders", Message::new("orders", vec![b'x'; 64]))
            .await
            .unwrap();

        assert!(
            consumer.recv().await.unwrap().is_none(),
            "超限消息不应交给业务"
        );
        let dead = service.dead_letters("orders").await.unwrap();
        assert_eq!(dead.len(), 1, "超限消息应转死信");
        assert_eq!(dead[0].attempts, 0, "转死信不算一次投递尝试");
    }
}
