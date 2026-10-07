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

use std::sync::Arc; // 引入 Arc，用于共享 Hub 与 Forwarder
use std::time::Duration; // 引入 Duration，用于重连退避间隔

use futures::StreamExt as _; // 引入 StreamExt 以使用 next() 拉取 Redis 消息流
use redis::aio::ConnectionManager; // 引入 redis 异步连接管理器（自动重连）

use super::hub::Hub; // 引入实时通信中心 Hub
use super::RealtimeMessage; // 引入实时消息类型

/// 转发器：发布侧句柄（订阅任务在 [`Forwarder::start`] 内拉起，随进程存活）
pub struct Forwarder { // 跨实例转发器：负责把本机消息 PUBLISH 到 Redis
    conn: ConnectionManager, // redis 异步连接管理器（发布用）
    channel: String, // 转发使用的 Redis channel 名
}

impl Forwarder { // Forwarder 实现：建连订阅与发布
    /// 建连并拉起订阅任务（坏地址在装配期报错，而非首条消息时）。
    /// `url` 复用 `[cache.redis].url`，channel 用 `[realtime].forward_topic`。
    pub async fn start(hub: Arc<Hub>, channel: String, url: &str) -> Result<Arc<Self>, String> { // 建立连接并启动订阅任务
        let client = redis::Client::open(url).map_err(|e| format!("bad redis url: {e}"))?; // 打开 Redis 客户端，地址非法即报错
        let conn = ConnectionManager::new(client.clone()) // 用客户端创建连接管理器
            .await // 等待连接建立
            .map_err(|e| format!("redis connect failed: {e}"))?; // 连接失败返回错误（装配期暴露）
        let forwarder = Arc::new(Self { conn, channel: channel.clone() }); // 构造转发器并共享
        spawn_subscriber(hub, client, channel); // 拉起后台订阅任务（本机重放入口）
        Ok(forwarder) // 返回可发布的转发器句柄
    }

    /// 发布到转发 channel（各实例的订阅任务本机重放）
    pub async fn publish(&self, msg: &RealtimeMessage) -> Result<(), String> { // 把消息发布到 Redis 转发 channel
        let mut conn = self.conn.clone(); // 克隆连接管理器（内部共享同一连接）
        redis::cmd("PUBLISH") // 构造 PUBLISH 命令
            .arg(&self.channel) // 指定转发 channel
            .arg(msg.to_json()) // 指定消息体（JSON 文本）
            .query_async::<i64>(&mut conn) // 异步执行并读取订阅者数量
            .await // 等待命令完成
            .map(|_| ()) // 丢弃返回值，只保留成功
            .map_err(|e| format!("redis PUBLISH failed: {e}")) // 失败转为可读错误
    }
}

/// 订阅循环：连接断开后 1s 退避重连
fn spawn_subscriber(hub: Arc<Hub>, client: redis::Client, channel: String) { // 后台任务：持续订阅并本机重放
    tokio::spawn(async move { // 派生常驻异步任务
        loop { // 无限重连循环
            if let Err(e) = run_subscription(&hub, &client, &channel).await { // 运行一次订阅，出错则记录
                tracing::warn!(error = %e, "realtime forward subscription lost, retrying in 1s"); // 告警订阅中断
            }
            tokio::time::sleep(Duration::from_secs(1)).await; // 退避 1 秒后重连
        }
    });
}

async fn run_subscription(hub: &Hub, client: &redis::Client, channel: &str) -> Result<(), String> { // 单次订阅会话：连上后转发消息直至断开
    let mut pubsub = client // 创建异步 pub/sub 连接
        .get_async_pubsub() // 获取 pub/sub 接口
        .await // 等待建立
        .map_err(|e| format!("redis pubsub connect failed: {e}"))?; // 连接失败返回错误
    pubsub // 订阅转发 channel
        .subscribe(channel) // 执行 SUBSCRIBE
        .await // 等待订阅完成
        .map_err(|e| format!("redis SUBSCRIBE failed: {e}"))?; // 订阅失败返回错误
    tracing::info!(channel, "realtime forward subscriber started"); // 记录订阅已启动
    let mut stream = pubsub.on_message(); // 取得消息流
    while let Some(msg) = stream.next().await { // 循环拉取转发消息
        let payload: String = match msg.get_payload() { // 读取消息载荷
            Ok(p) => p, // 读取成功
            Err(e) => { // 载荷不可读（非 UTF-8 等）
                tracing::warn!(error = %e, "realtime forward payload unreadable, dropped"); // 告警并丢弃
                continue; // 跳过本条
            }
        };
        match serde_json::from_str::<RealtimeMessage>(&payload) { // 解析为实时消息
            Ok(m) => { // 解析成功
                hub.broadcast_local(&m); // 本机重放给订阅该频道的连接
            }
            Err(e) => { // 解析失败（损坏消息）
                tracing::warn!(error = %e, "realtime forward message corrupt, dropped"); // 告警并丢弃
            }
        }
    }
    Err("pubsub stream ended".to_string()) // 流结束视为异常，触发外层退避重连
}
