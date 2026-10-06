//! 跨实例转发（文档 三·15）：v1 经 **Redis Pub/Sub**——真广播语义（每个实例
//! 各收一份），与任务队列的消费组语义（同组分摊、一条消息一个实例处理）正交，
//! 因此**不复用 Queue trait**；经 queue 消费组转发列为演进项（需为各后端补
//! per-instance 消费组/队列）。
//!
//! - 发布：`Hub::broadcast` 在转发启用时 `PUBLISH` 到转发 channel
//!   （失败降级本机广播并告警）；
//! - 订阅：[`Forwarder::start`] 拉起后台任务，`SUBSCRIBE` 转发 channel，
//!   消息解析为 `RealtimeMessage` 后 `Hub::broadcast_local` 本机重放；
//!   断线 1s 退避自动重连，损坏消息按丢弃处理；
//! - 启用条件：feature = "cache-redis" + `[cache].backend = "redis"` +
//!   `[realtime].forward = "queue"`（单机部署无需开启）。

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use redis::aio::ConnectionManager;

use super::hub::Hub;
use super::RealtimeMessage;

/// 转发器：发布侧句柄（订阅任务在 [`Forwarder::start`] 内拉起，随进程存活）
pub struct Forwarder {
    conn: ConnectionManager,
    channel: String,
}

impl Forwarder {
    /// 建连并拉起订阅任务（坏地址在装配期报错，而非首条消息时）。
    /// `url` 复用 `[cache.redis].url`，channel 用 `[realtime].forward_topic`。
    pub async fn start(hub: Arc<Hub>, channel: String, url: &str) -> Result<Arc<Self>, String> {
        let client = redis::Client::open(url).map_err(|e| format!("bad redis url: {e}"))?;
        let conn = ConnectionManager::new(client.clone())
            .await
            .map_err(|e| format!("redis connect failed: {e}"))?;
        let forwarder = Arc::new(Self { conn, channel: channel.clone() });
        spawn_subscriber(hub, client, channel);
        Ok(forwarder)
    }

    /// 发布到转发 channel（各实例的订阅任务本机重放）
    pub async fn publish(&self, msg: &RealtimeMessage) -> Result<(), String> {
        let mut conn = self.conn.clone();
        redis::cmd("PUBLISH")
            .arg(&self.channel)
            .arg(msg.to_json())
            .query_async::<i64>(&mut conn)
            .await
            .map(|_| ())
            .map_err(|e| format!("redis PUBLISH failed: {e}"))
    }
}

/// 订阅循环：连接断开后 1s 退避重连
fn spawn_subscriber(hub: Arc<Hub>, client: redis::Client, channel: String) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_subscription(&hub, &client, &channel).await {
                tracing::warn!(error = %e, "realtime forward subscription lost, retrying in 1s");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

async fn run_subscription(hub: &Hub, client: &redis::Client, channel: &str) -> Result<(), String> {
    let mut pubsub = client
        .get_async_pubsub()
        .await
        .map_err(|e| format!("redis pubsub connect failed: {e}"))?;
    pubsub
        .subscribe(channel)
        .await
        .map_err(|e| format!("redis SUBSCRIBE failed: {e}"))?;
    tracing::info!(channel, "realtime forward subscriber started");
    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "realtime forward payload unreadable, dropped");
                continue;
            }
        };
        match serde_json::from_str::<RealtimeMessage>(&payload) {
            Ok(m) => {
                hub.broadcast_local(&m);
            }
            Err(e) => {
                tracing::warn!(error = %e, "realtime forward message corrupt, dropped");
            }
        }
    }
    Err("pubsub stream ended".to_string())
}
