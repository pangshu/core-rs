//! queue_worker —— M5 示例：队列生产 / 消费 + 重试 + 死信重放。
//!
//! 运行：`cargo run --example queue_worker --features cache,queue`
//!
//! 演示：
//! - `producer.publish` 投递消息（`message_id` 即幂等键）
//! - `consumer.recv` → 业务处理 → `ack` / `nack`
//! - `nack` 按 `[queue].retry` 退避重投，耗尽后入死信（`[queue].dlq`）
//! - `dead_letters` 查询 + `replay_dead` 重放
//!
//! 说明：默认后端为 `memory`（进程内）；多实例需切 `queue-redis` 等
//! （尚未实现，见模块级 TODO）。

use aurora::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = preset::load_env_var("AURORA_ENV", "config")?;
    // 示例：把重试次数调小，便于观察「重试 → 死信」
    config.queue.retry.max_attempts = 2;

    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    let producer = state.queue.producer("orders");
    let consumer = state.queue.consumer("orders");

    // 1) 投递三条消息
    for n in 1..=3 {
        let id = producer.publish(format!("order-{n}")).await?;
        tracing::info!(%id, "已投递");
    }
    tracing::info!(pending = state.queue.len("orders").await?, "队列深度");

    // 2) 消费：order-2 首次处理失败 → nack（退避重投）→ 第二次成功
    while let Some(message) = consumer.recv().await? {
        let payload = String::from_utf8_lossy(&message.payload).to_string();

        if payload == "order-2" && message.attempts == 0 {
            tracing::warn!(%payload, "模拟处理失败，nack（将退避重投）");
            consumer.nack(message).await?;
            continue;
        }

        tracing::info!(%payload, attempts = message.attempts, "处理成功，ack");
        consumer.ack(&message).await?;
    }

    // 3) 制造一条必失败的消息，验证死信（重试耗尽后不再重投，循环自然结束）
    producer.publish("poison").await?;
    while let Some(message) = consumer.recv().await? {
        let payload = String::from_utf8_lossy(&message.payload).to_string();
        tracing::warn!(%payload, attempts = message.attempts, "始终失败，nack");
        consumer.nack(message).await?;
    }

    let dead = state.queue.dead_letters("orders").await?;
    tracing::info!(count = dead.len(), "死信数量");

    // 4) 重放死信
    let replayed = state.queue.replay_dead("orders", 10).await?;
    tracing::info!(replayed, "已重放死信");
    tracing::info!(pending = state.queue.len("orders").await?, "重放后队列深度");

    tracing::info!("示例结束");
    Ok(())
}
