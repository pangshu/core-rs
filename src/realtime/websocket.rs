//! WebSocket 端点（feature `ws`）。
//!
//! 建连顺序（每一步失败都在升级**之前**返回 HTTP 错误，不消耗连接名额）：
//! `Origin 白名单 → 频道名校验 → 订阅授权 → 连接数上限 → 订阅`。
//!
//! 连接治理（`docs/03` §4.20）：连接数上限（全局 + 每 IP）、单帧/单消息**大小上限**、
//! 上行**速率上限**、心跳超时、出站**写超时**；服务关闭时发 `Close(1001)`。

use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path, Request};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::config::InboundOverflow;
use crate::errors::{AppError, ErrorCode};
use crate::realtime::hub::{Hub, SubscribeRequest, Subscription};
use crate::realtime::message::{InboundMessage, MessageKind, RealtimeMessage};
use crate::realtime::topic;
use crate::utils::ClientIp;

/// `1013 Try Again Later`：上行队列溢出时用它关闭连接（axum 未提供该常量）。
const CLOSE_TRY_AGAIN_LATER: u16 = 1013;

/// 构造 WebSocket 处理函数（路由 `/ws/{topic}`）。
///
/// ```ignore
/// let hub = Hub::from_config(&config.realtime);
/// hub.on_subscribe(|req| /* 频道归属校验 */ true);
/// Router::new().route("/ws/{room}", get(ws::handler(hub.clone())))
/// ```
pub fn handler(
    hub: Hub,
) -> impl Fn(WebSocketUpgrade, Path<String>, ClientIp, Request) -> BoxFuture<'static, Response>
+ Clone
+ Send
+ Sync
+ 'static {
    move |upgrade: WebSocketUpgrade,
          Path(topic): Path<String>,
          ClientIp(ip): ClientIp,
          request: Request| {
        let hub = hub.clone();
        Box::pin(async move {
            // ① Origin 白名单（跨站 WebSocket 劫持）：只在请求**带了** Origin 时校验
            if let Err(error) = check_origin(&hub, request.headers()) {
                return error.into_response();
            }
            // ② 频道名：来自 URL 且已 percent-decode，必须收紧（日志注入 / 频道表放大）
            if let Err(error) = topic::validate(&topic, hub.config().max_topic_len) {
                tracing::warn!(
                    topic = %topic::log_view(&topic),
                    "WebSocket 频道名非法，拒绝升级"
                );
                return error.into_response();
            }
            // ③ 订阅授权：必须在**占用连接名额之前**，否则未授权请求可以先把名额打满
            if let Err(error) = hub.authorize_subscribe(&SubscribeRequest {
                topic: &topic,
                peer: ip,
                headers: request.headers(),
                extensions: request.extensions(),
            }) {
                tracing::warn!(
                    topic = %topic::log_view(&topic),
                    "WebSocket 订阅被拒绝"
                );
                return error.into_response();
            }
            // ④ 连接名额（每 IP + 每认证主体）
            let subject = crate::realtime::hub::subject_from_extensions(request.extensions());
            let guard = match hub.acquire_connection_for(ip, subject.as_deref()) {
                Ok(guard) => guard,
                Err(error) => return error.into_response(),
            };
            // ⑤ 订阅频道
            let subscription = match hub.subscribe(&topic) {
                Ok(subscription) => subscription,
                Err(error) => return error.into_response(),
            };

            let max_size = hub.config().max_message_size_bytes();
            upgrade
                .max_message_size(max_size)
                .max_frame_size(max_size)
                .on_upgrade(move |socket| async move {
                    // guard 随连接任务结束而释放（归还名额）。
                    let _guard = guard;
                    run(socket, hub, subscription, topic).await;
                })
                .into_response()
        })
    }
}

/// `Origin` 白名单校验。
///
/// 浏览器发起 WebSocket 握手**必然**带 `Origin`，而非浏览器客户端不带；CSWSH 只可能
/// 来自浏览器，因此「没带 `Origin`」放行、「带了但不在白名单」拒绝。
fn check_origin(hub: &Hub, headers: &crate::axum::http::HeaderMap) -> Result<(), AppError> {
    let allowed = &hub.config().allowed_origins;
    if allowed.is_empty() {
        return Ok(());
    }
    let Some(origin) = headers
        .get(crate::axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    let origin = origin.trim_end_matches('/');
    if allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(origin))
    {
        return Ok(());
    }
    tracing::warn!(
        origin = %crate::utils::string::log_safe(origin, 128),
        "WebSocket 升级的 Origin 不在白名单内，拒绝"
    );
    Err(AppError::biz(
        ErrorCode::FORBIDDEN,
        "该来源不允许建立实时连接",
    ))
}

/// 单连接主循环：出站转发广播、入站处理心跳/关闭、定时探测心跳超时。
async fn run(socket: WebSocket, hub: Hub, mut receiver: Subscription, topic: String) {
    let (mut sink, mut stream) = socket.split();
    let heartbeat_interval = hub.config().heartbeat_interval;
    let heartbeat_timeout = hub.config().heartbeat_timeout;
    // 出站写超时：客户端「只连不读」时 `sink.send()` 会一直挂着，而它是在
    // `select!` 的**分支体内** await 的——挂住期间心跳分支根本不会再被轮询，
    // `heartbeat_timeout` 形同虚设，连接与名额被无限期占用。
    let write_timeout = hub.config().write_timeout;
    let topic_log = topic::log_view(&topic);

    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    let mut last_seen = Instant::now();

    // 上行队列：处理器在独立任务里跑，读循环不被慢处理器拖住（否则心跳也会耽误）。
    // `inbound_buffer = 0` 时保持旧行为（同步直调）。
    let mut inbound = InboundQueue::new(&hub, &topic);
    // 因队列溢出而退出：用 1013 而不是 1001，让客户端知道「是你发太快」而不是「服务端要关」
    let mut overflow = false;
    // 上行限速（条/秒）：一条上行帧可能被处理器广播给 N 个订阅者，不限速就是放大器
    let mut gate = RateGate::new(hub.config().inbound_rate_per_sec);

    loop {
        tokio::select! {
            // 出站：频道广播 → 客户端
            broadcast = receiver.recv() => {
                match broadcast {
                    Ok(message) => {
                        if send_message(&mut sink, &message, write_timeout).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(topic = %topic_log, skipped, "WebSocket 订阅端落后，已跳过部分消息");
                    }
                    // Hub::shutdown() 丢弃 sender 时到这里 → 优雅关闭
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // 入站：心跳 / 关闭 / 上行
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        last_seen = Instant::now();
                        if send_frame(&mut sink, Message::Pong(payload), write_timeout).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => last_seen = Instant::now(),
                    Some(Ok(Message::Text(text))) => {
                        last_seen = Instant::now();
                        if !gate.allow() {
                            tracing::warn!(topic = %topic_log, "上行速率超限，丢弃该帧");
                            continue;
                        }
                        // 业务上行：交给 `Hub::on_message` 注册的处理器
                        if inbound
                            .push(InboundMessage::new(
                                topic.clone(),
                                MessageKind::Text,
                                text.as_str().as_bytes().to_vec(),
                            ))
                            .is_err()
                        {
                            overflow = true;
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        last_seen = Instant::now();
                        if !gate.allow() {
                            tracing::warn!(topic = %topic_log, "上行速率超限，丢弃该帧");
                            continue;
                        }
                        if inbound
                            .push(InboundMessage::new(
                                topic.clone(),
                                MessageKind::Binary,
                                bytes.to_vec(),
                            ))
                            .is_err()
                        {
                            overflow = true;
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        tracing::debug!(topic = %topic_log, %error, "WebSocket 读失败");
                        break;
                    }
                }
            }

            // 心跳：超时则断开，否则主动 Ping
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > heartbeat_timeout {
                    tracing::debug!(topic = %topic_log, "心跳超时，断开连接");
                    break;
                }
                if send_frame(&mut sink, Message::Ping(Vec::new().into()), write_timeout).await.is_err() {
                    break;
                }
            }
        }
    }

    // 放掉队列：消费者任务随之退出
    drop(inbound);

    // 发关闭帧（1013 = 上行过快；1001 = going away），尽力而为。
    let (code, reason) = if overflow {
        (CLOSE_TRY_AGAIN_LATER, "inbound queue overflow")
    } else {
        (close_code::AWAY, "server closing")
    };
    let _ = send_frame(
        &mut sink,
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
        write_timeout,
    )
    .await;
    let _ = tokio::time::timeout(write_timeout, sink.flush()).await;

    // 等客户端回 Close 再关 TCP（RFC 6455 §7.1.1 的关闭握手）。
    //
    // 不等的话：socket 里还有客户端发来的未读数据，直接 close 会让内核发 **RST**，
    // 把刚发出的 Close 帧冲掉——客户端只能看到 `1006 连接异常`，拿不到 `1013`/`1001`。
    // 上限 1s，避免客户端不配合时把连接拖住。
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while let Some(Ok(frame)) = stream.next().await {
            if matches!(frame, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

/// 带写超时的帧发送：`Err(())` 表示「该断开连接」。
async fn send_frame(
    sink: &mut SplitSink<WebSocket, Message>,
    frame: Message,
    timeout: Duration,
) -> Result<(), ()> {
    match tokio::time::timeout(timeout, sink.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(()),
        Err(_) => {
            // 客户端不读（接收窗口塞满）→ 主动断开，把连接名额还回去
            tracing::warn!("WebSocket 出站写超时，判定为慢消费者并断开");
            Err(())
        }
    }
}

/// 广播消息 → WS 帧。
async fn send_message(
    sink: &mut SplitSink<WebSocket, Message>,
    message: &RealtimeMessage,
    timeout: Duration,
) -> Result<(), ()> {
    let frame = match message.kind {
        MessageKind::Binary => Message::Binary(message.payload.clone().into_bytes().into()),
        MessageKind::Text | MessageKind::Json => Message::Text(message.payload.clone().into()),
    };
    send_frame(sink, frame, timeout).await
}

/// 上行速率闸门（令牌桶，突发量 = 速率）。
///
/// 一条上行帧往往会被业务处理器广播给该频道的全部订阅者——不限速时一个客户端就能
/// 把 N 个订阅者的带宽放大成 N 倍。
struct RateGate {
    /// 每秒补充的令牌数；`0` 表示不限速。
    per_sec: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl RateGate {
    fn new(per_sec: u32) -> Self {
        let per_sec = f64::from(per_sec);
        // 突发量至少 32：小速率（如 1/s）不该把「连接后立刻发一批」全部拒掉
        let capacity = per_sec.max(32.0);
        Self {
            per_sec,
            capacity,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        if self.per_sec <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

/// 上行消息队列（有界）。
///
/// 容量为 0 时退化为**同步直调**（旧行为）。否则读循环只做 `try_send`，
/// 处理器在独立任务里消费——慢处理器不会阻塞心跳与关闭帧。
///
/// 队列满时按 [`InboundOverflow`] 处理：
/// - `close`：返回 `Err`，由读循环断开连接（`1013`）
/// - `drop`：丢弃该帧并计数，每 100 条告警一次（避免日志被刷爆）
struct InboundQueue {
    /// 容量 0 时为空，此时走同步直调（需要 `hub`）。
    sender: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    hub: Hub,
    policy: InboundOverflow,
    dropped: usize,
}

impl InboundQueue {
    fn new(hub: &Hub, topic: &str) -> Self {
        let policy = hub.config().inbound_overflow;
        let capacity = hub.config().inbound_buffer;

        if capacity == 0 {
            return Self {
                sender: None,
                hub: hub.clone(),
                policy,
                dropped: 0,
            };
        }

        let (sender, mut receiver) = tokio::sync::mpsc::channel(capacity);
        let consumer = hub.clone();
        let topic = topic::log_view(topic);
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                consumer.dispatch(message);
            }
            tracing::debug!(%topic, "上行队列消费者退出");
        });

        Self {
            sender: Some(sender),
            hub: hub.clone(),
            policy,
            dropped: 0,
        }
    }

    /// 投递一帧；`Err(())` 表示「应当断开连接」。
    fn push(&mut self, message: InboundMessage) -> Result<(), ()> {
        let Some(sender) = &self.sender else {
            // 容量 0：同步直调（保持旧行为），处理器在这里直接执行
            self.hub.dispatch(message);
            return Ok(());
        };

        match sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => match self.policy {
                InboundOverflow::Close => {
                    tracing::warn!("上行队列已满，断开连接（1013）");
                    Err(())
                }
                InboundOverflow::Drop => {
                    self.dropped += 1;
                    if self.dropped == 1 || self.dropped.is_multiple_of(100) {
                        tracing::warn!(dropped = self.dropped, "上行队列已满，丢弃新帧");
                    }
                    Ok(())
                }
            },
            // 消费者已退出（不应发生：队列随连接存活）
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }
}

// 业务上行消息由 `Hub::on_message` 注册的处理器接收（见 `docs/03` §4.20）。
// 容量 0 时处理器在读循环里**同步**执行，耗时逻辑请自行 `tokio::spawn`。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RealtimeSection;

    fn hub_with_origins(origins: &[&str]) -> Hub {
        Hub::from_config(&RealtimeSection {
            allowed_origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            ..RealtimeSection::default()
        })
    }

    fn headers_with_origin(origin: Option<&str>) -> crate::axum::http::HeaderMap {
        let mut headers = crate::axum::http::HeaderMap::new();
        if let Some(origin) = origin {
            headers.insert(
                crate::axum::http::header::ORIGIN,
                origin.parse().expect("合法 header 值"),
            );
        }
        headers
    }

    #[test]
    fn should_skip_origin_check_when_allowlist_empty() {
        let hub = hub_with_origins(&[]);
        assert!(
            check_origin(&hub, &headers_with_origin(Some("https://evil.example"))).is_ok(),
            "未配置白名单时保持旧行为（兼容）"
        );
    }

    #[test]
    fn should_allow_non_browser_client_without_origin() {
        let hub = hub_with_origins(&["https://good.example"]);
        assert!(
            check_origin(&hub, &headers_with_origin(None)).is_ok(),
            "非浏览器客户端不带 Origin；CSWSH 只可能来自浏览器"
        );
    }

    #[test]
    fn should_reject_foreign_origin() {
        let hub = hub_with_origins(&["https://good.example"]);
        let error = check_origin(&hub, &headers_with_origin(Some("https://evil.example")))
            .expect_err("跨站 Origin 必须被拒");
        assert_eq!(error.code(), ErrorCode::FORBIDDEN);
    }

    #[test]
    fn should_match_origin_case_insensitively_and_ignore_trailing_slash() {
        let hub = hub_with_origins(&["https://Good.Example"]);
        assert!(check_origin(&hub, &headers_with_origin(Some("https://good.example/"))).is_ok());
    }

    #[test]
    fn should_enforce_inbound_rate() {
        // 突发量 = max(rate, 32)，因此 2/s 的桶也是 32 发
        let mut gate = RateGate::new(2);
        for index in 0..32 {
            assert!(gate.allow(), "第 {index} 发应放行");
        }
        assert!(!gate.allow(), "突发用尽后应拒绝");
    }

    #[test]
    fn should_not_limit_when_rate_is_zero() {
        let mut gate = RateGate::new(0);
        for _ in 0..1_000 {
            assert!(gate.allow(), "0 = 不限制");
        }
    }
}
