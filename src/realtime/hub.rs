//! 频道广播中心（[`Hub`]）。
//!
//! 每频道一条 `tokio::sync::broadcast` 通道；订阅者（WS/SSE 连接）各自持有
//! [`Subscription`]。同时负责**订阅授权**、**连接数治理**（全局 + 每 IP）、
//! **频道表生命周期**与**优雅关闭**。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, Weak};

use tokio::sync::broadcast;

use crate::axum::http::{Extensions, HeaderMap};
use crate::config::RealtimeSection;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::realtime::message::{InboundMessage, RealtimeMessage};
use crate::realtime::topic;

/// 上行消息处理器（业务注册）。
pub type InboundHandler = Arc<dyn Fn(InboundMessage) + Send + Sync + 'static>;

/// 订阅授权钩子（业务注册）。
///
/// 入参是**升级 / 建流请求**的频道名、对端地址、请求头与扩展——`auth` feature 开启时
/// 身份（[`Identity`](crate::auth::Identity)）已由认证中间件写进扩展，钩子直接
/// `extensions.get::<Identity>()` 取用即可。
///
/// **未注册 = fail-closed**：端点会拒绝订阅（`50001`），而不是默认放行。
pub type SubscribeAuthorizer = Arc<dyn Fn(&SubscribeRequest<'_>) -> bool + Send + Sync + 'static>;

/// 订阅授权上下文。
#[derive(Debug)]
pub struct SubscribeRequest<'a> {
    /// 客户端请求的频道名（**已通过 [`topic::validate`] 校验**）。
    pub topic: &'a str,
    /// 客户端 IP（来源不可知时为 `None`）。
    pub peer: Option<IpAddr>,
    /// 握手请求头（可读 `Authorization` / `Origin` / `Sec-WebSocket-Protocol` 等）。
    pub headers: &'a HeaderMap,
    /// 请求扩展（认证中间件写入的 `Identity` 等）。
    pub extensions: &'a Extensions,
}

/// 每频道的广播缓冲条数（`docs/08` 未定义，取 256；落后过多的订阅者会收到
/// `Lagged` 并被跳过，见各连接实现）。
pub const TOPIC_BUFFER: usize = 256;

/// 来源不可知（无 `ConnectInfo`）时使用的桶地址。
///
/// **fail-closed**：这类连接不是「不受限」，而是**共用一份** `max_connections_per_ip`
/// 配额。此前 `ip = None` 直接跳过每 IP 校验，等于把限流关掉。
const UNKNOWN_PEER: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// 频道表条目。
#[derive(Debug)]
struct TopicEntry {
    sender: broadcast::Sender<Arc<RealtimeMessage>>,
    /// 最近一次被订阅 / 发布的序号（`max_topics` 达上限时淘汰最久未用的空闲频道）。
    last_used: u64,
}

/// 频道广播中心。
///
/// 由业务自行持有并 `clone()` 进路由（见 `docs/07` §3.20）：
/// ```no_run
/// use aurora::config::AppConfig;
/// use aurora::realtime::Hub;
///
/// let config = AppConfig::default();
/// let hub = Hub::from_config(&config.realtime);
/// // 业务侧自行 clone 进路由（`realtime::handle` 会一次性挂载 WS 与 SSE）
/// let _shared = hub.clone();
/// ```
#[derive(Clone)]
pub struct Hub {
    config: RealtimeSection,
    topics: Arc<Mutex<HashMap<String, TopicEntry>>>,
    /// 频道「最近使用」单调序号。
    seq: Arc<AtomicU64>,
    connections: Arc<Connections>,
    closed: Arc<AtomicBool>,
    /// 实例标识（跨实例转发时用于丢弃自己发的消息）。
    instance: Arc<str>,
    /// 上行消息处理器（业务注册；未注册时入站业务帧只更新心跳）。
    handler: Arc<std::sync::RwLock<Option<InboundHandler>>>,
    /// 订阅授权钩子（业务注册；未注册 = fail-closed 拒绝订阅）。
    authorizer: Arc<std::sync::RwLock<Option<SubscribeAuthorizer>>>,
    /// 跨实例转发上下文（`[realtime].forward_via_queue` + `queue` + `cache`）。
    #[cfg(all(feature = "queue", feature = "cache"))]
    forwarder: Arc<Mutex<Option<crate::realtime::forward::Forwarder>>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("topics", &self.topic_count())
            .field("connections", &self.connection_count())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

/// 取锁；**中毒按恢复处理**而不是永久失败。
///
/// 计数表 / 频道表被 panic 污染时，里面的数据仍然是自洽的（只是有人持锁时崩了）。
/// 此前这里返回 `Err`，调用方 `?` 早退——`acquire_connection` 在 `fetch_add` 之后
/// 早退会把名额永久泄漏，最终**所有新连接 503 且无法自愈**。
fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(what, "实时模块的锁已中毒，按恢复处理（数据仍自洽）");
            poisoned.into_inner()
        }
    }
}

impl Hub {
    /// 以配置创建。
    #[must_use]
    pub fn from_config(config: &RealtimeSection) -> Self {
        Self {
            config: config.clone(),
            topics: Arc::new(Mutex::new(HashMap::new())),
            seq: Arc::new(AtomicU64::new(0)),
            connections: Arc::new(Connections::default()),
            closed: Arc::new(AtomicBool::new(false)),
            instance: Arc::from(uuid::Uuid::new_v4().to_string()),
            handler: Arc::new(std::sync::RwLock::new(None)),
            authorizer: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(all(feature = "queue", feature = "cache"))]
            forwarder: Arc::new(Mutex::new(None)),
        }
    }

    /// 以默认配置创建。
    #[must_use]
    pub fn new() -> Self {
        Self::from_config(&RealtimeSection::default())
    }

    /// 当前配置。
    #[must_use]
    pub fn config(&self) -> &RealtimeSection {
        &self.config
    }

    /// 本实例标识（跨实例转发用）。
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    /// 注册上行消息处理器（后注册覆盖先注册）。
    ///
    /// 处理器在 WebSocket 读循环里**同步**调用，应尽快返回；耗时逻辑请自行
    /// `tokio::spawn`，并用 `hub.publish()` 广播结果。
    pub fn on_message(&self, handler: impl Fn(InboundMessage) + Send + Sync + 'static) {
        if let Ok(mut slot) = self.handler.write() {
            *slot = Some(Arc::new(handler));
        }
    }

    /// 注册订阅授权钩子（后注册覆盖先注册）。
    ///
    /// **不注册就等于拒绝一切订阅**（fail-closed）。钩子必须回答「这个请求能否订阅
    /// 这个频道」，而不是「这个请求是否合法」——频道名通常是 `user:123` 这类可猜的
    /// 业务 ID，只校验身份不校验归属，等于把别人的频道公开。
    ///
    /// ```no_run
    /// use aurora::realtime::Hub;
    ///
    /// let hub = Hub::new();
    /// hub.on_subscribe(|req| {
    ///     // 示例：只允许订阅以自己 subject 结尾的频道
    ///     req.topic.starts_with("public:") || req.topic.ends_with(":me")
    /// });
    /// ```
    pub fn on_subscribe(
        &self,
        authorize: impl Fn(&SubscribeRequest<'_>) -> bool + Send + Sync + 'static,
    ) {
        if let Ok(mut slot) = self.authorizer.write() {
            *slot = Some(Arc::new(authorize));
        }
    }

    /// 是否已注册订阅授权钩子。
    #[must_use]
    pub fn has_subscribe_authorizer(&self) -> bool {
        self.authorizer
            .read()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    /// 校验订阅授权（端点在建连 / 建流前调用）。
    ///
    /// # Errors
    /// 钩子返回 `false` → `20002`；**未注册钩子** → `50001`（配置缺失，不是客户端问题）。
    pub fn authorize_subscribe(&self, request: &SubscribeRequest<'_>) -> AppResult<()> {
        let guard = self
            .authorizer
            .read()
            .map_err(|_| AppError::internal(anyhow::anyhow!("实时订阅授权表被污染")))?;
        match guard.as_ref() {
            Some(authorize) if authorize(request) => Ok(()),
            Some(_) => Err(AppError::biz(ErrorCode::FORBIDDEN, "无权订阅该实时频道")),
            None => Err(AppError::internal(anyhow::anyhow!(
                "未注册订阅授权钩子：请先调用 Hub::on_subscribe（fail-closed 拒绝）"
            ))),
        }
    }

    /// 分发上行消息（由 WebSocket 读循环调用）。
    #[cfg(feature = "ws")]
    pub(crate) fn dispatch(&self, inbound: InboundMessage) {
        let handler = self
            .handler
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone));
        match handler {
            Some(handler) => handler(inbound),
            None => {
                tracing::debug!(
                    topic = %topic::log_view(&inbound.topic),
                    "未注册上行处理器，忽略业务上行消息"
                );
            }
        }
    }

    /// 启用跨实例转发（`[realtime].forward_via_queue = true`，需 `queue` + `cache`）。
    ///
    /// - `queue`：实例间传输通道（每实例一个队列）
    /// - `cache`：实例注册表（JSON map + TTL 心跳），用于发现 peer
    ///
    /// 未开启 `forward_via_queue` 时原样返回，不启动任何任务。
    #[cfg(all(feature = "queue", feature = "cache"))]
    #[must_use]
    pub fn with_forwarding(
        self,
        queue: crate::queue::QueueService,
        cache: crate::cache::CacheService,
    ) -> Self {
        if !self.config.forward_via_queue {
            return self;
        }

        let registry = crate::realtime::forward::Registry::new(cache, self.instance.to_string());
        let heartbeat = registry.spawn_heartbeat();

        let forwarder = crate::realtime::forward::Forwarder {
            queue: queue.clone(),
            registry,
            instance: Arc::clone(&self.instance),
            heartbeat,
        };
        *lock_or_recover(&self.forwarder, "实时转发上下文") = Some(forwarder);

        // 消费者持有 Hub 克隆；`shutdown()` 后自行退出
        crate::realtime::forward::spawn_consumer(self.clone(), queue);
        self
    }

    /// 投递到 peer 队列（尽力而为：失败只告警）。
    #[cfg(all(feature = "queue", feature = "cache"))]
    fn forward(&self, topic: &str, message: RealtimeMessage) {
        let forwarder = lock_or_recover(&self.forwarder, "实时转发上下文").clone();
        let Some(forwarder) = forwarder else {
            return;
        };

        let topic = topic.to_string();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { forwarder.fanout(&topic, message).await });
            }
            Err(_) => tracing::warn!("当前不在 tokio 运行时内，跳过实时转发"),
        }
    }

    /// 向频道广播，返回**接收端数量**（无订阅者时为 0，不算失败）。
    ///
    /// # Errors
    /// 已调用 [`shutdown`](Self::shutdown) 时返回 `50005`；频道名非法返回 `10001`；
    /// payload 超过 `max_message_size` 返回 `10002`。
    pub fn publish(&self, topic: &str, message: RealtimeMessage) -> AppResult<usize> {
        let delivered = self.publish_local(topic, message.clone())?;
        #[cfg(all(feature = "queue", feature = "cache"))]
        self.forward(topic, message);
        Ok(delivered)
    }

    /// 只做本地广播（转发消费者用它，避免回环）。
    ///
    /// **不会创建频道**：没有订阅者的频道直接返回 0。此前这里 `or_insert_with`
    /// 会为每个发布过的频道永久留一条 256 槽环形缓冲（且常驻最后 256 条 payload），
    /// 是内存无界增长的一半来源。
    ///
    /// # Errors
    /// 已调用 [`shutdown`](Self::shutdown) 时返回 `50005`；频道名非法返回 `10001`；
    /// payload 超过 `max_message_size` 返回 `10002`。
    pub fn publish_local(&self, topic: &str, mut message: RealtimeMessage) -> AppResult<usize> {
        if self.is_closed() {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "实时通道已关闭",
            ));
        }
        topic::validate(topic, self.config.max_topic_len)?;

        // 出站同样受 `max_message_size` 约束：`send_message` / `to_event` 对**每个**
        // 订阅者都要克隆一份 payload，一条大消息 × N 订阅者就是一次内存放大。
        let limit = self.config.max_message_size_bytes();
        if message.payload.len() > limit {
            return Err(AppError::biz(
                ErrorCode::PAYLOAD_TOO_LARGE,
                format!("实时消息超过上限（{limit} 字节）"),
            ));
        }
        message.topic = topic.to_string();

        let sender = {
            let mut topics = lock_or_recover(&self.topics, "实时频道表");
            let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
            topics.get_mut(topic).map(|entry| {
                entry.last_used = seq;
                entry.sender.clone()
            })
        };
        let Some(sender) = sender else {
            return Ok(0);
        };
        // `send` 在「无接收者」时报错——广播语义下不算失败。
        Ok(sender.send(Arc::new(message)).unwrap_or(0))
    }

    /// 订阅频道（每个连接调用一次）。
    ///
    /// 返回的 [`Subscription`] **drop 即注销**：最后一个订阅者离开时频道条目会被摘除，
    /// 频道表不会随着「连过又走」的随机频道名无限增长。
    ///
    /// # Errors
    /// 已关闭（`50005`）、频道名非法（`10001`）、频道数达 `max_topics` 且无空闲频道可
    /// 淘汰（`50005`）。
    pub fn subscribe(&self, topic: &str) -> AppResult<Subscription> {
        if self.is_closed() {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "实时通道已关闭",
            ));
        }
        topic::validate(topic, self.config.max_topic_len)?;

        let mut topics = lock_or_recover(&self.topics, "实时频道表");
        if !topics.contains_key(topic) {
            if topics.len() >= self.config.max_topics {
                evict_idle(&mut topics);
                if topics.len() >= self.config.max_topics {
                    tracing::warn!(
                        max_topics = self.config.max_topics,
                        "实时频道数已达上限且无空闲频道可淘汰，拒绝新订阅"
                    );
                    return Err(AppError::biz(
                        ErrorCode::SERVICE_UNAVAILABLE,
                        "实时频道数已达上限",
                    ));
                }
            }
            let (sender, _) = broadcast::channel(TOPIC_BUFFER);
            topics.insert(
                topic.to_string(),
                TopicEntry {
                    sender,
                    last_used: 0,
                },
            );
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = topics.get_mut(topic).expect("上面刚插入或已存在");
        entry.last_used = seq;
        let receiver = entry.sender.subscribe();
        drop(topics);

        Ok(Subscription {
            receiver: Some(receiver),
            topics: Arc::downgrade(&self.topics),
            topic: topic.to_string(),
        })
    }

    /// 申请一个连接名额（`ConnectionGuard` drop 时自动归还）。
    ///
    /// 超限返回 `50005`（503）——连接数属于容量问题。
    ///
    /// 来源不可知（`ip = None`）时**不跳过每 IP 限制**，而是归入「未知来源」桶与其它
    /// 未知来源共用一份配额（fail-closed）。
    ///
    /// # Errors
    /// 已关闭或超出 `max_connections` / `max_connections_per_ip` 时返回 `50005`。
    pub fn acquire_connection(&self, ip: Option<IpAddr>) -> AppResult<ConnectionGuard> {
        self.acquire_connection_for(ip, None)
    }

    /// 同 [`acquire_connection`](Self::acquire_connection)，但额外按认证主体计数
    /// （`[realtime].max_connections_per_subject`）。
    ///
    /// # Errors
    /// 另加「该主体连接数已达上限」时返回 `50005`。
    pub fn acquire_connection_for(
        &self,
        ip: Option<IpAddr>,
        subject: Option<&str>,
    ) -> AppResult<ConnectionGuard> {
        if self.is_closed() {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "实时通道已关闭",
            ));
        }

        let bucket = match ip {
            Some(ip) => ip,
            None => {
                warn_unknown_peer();
                UNKNOWN_PEER
            }
        };
        let subject = subject.map(clamp_subject);

        // 先拿锁再计数：反过来（先 `fetch_add` 再 `lock()?`）一旦取锁失败就会把
        // 名额永久泄漏出去。锁中毒也已改为恢复处理，这里不再有早退分支。
        // 加锁顺序固定为 `per_ip → per_subject`（`ConnectionGuard::drop` 同序），
        // 因此不会死锁。
        let mut per_ip = lock_or_recover(&self.connections.per_ip, "实时连接计数");
        if per_ip.get(&bucket).copied().unwrap_or(0) >= self.config.max_connections_per_ip as usize
        {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "该 IP 的实时连接数已达上限",
            ));
        }

        let mut per_subject = lock_or_recover(&self.connections.per_subject, "实时主体连接计数");
        if let Some(key) = subject.as_deref()
            && self.config.max_connections_per_subject > 0
            && per_subject.get(key).copied().unwrap_or(0)
                >= self.config.max_connections_per_subject as usize
        {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "该账号的实时连接数已达上限",
            ));
        }

        let total = self.connections.total.fetch_add(1, Ordering::AcqRel) + 1;
        if total > self.config.max_connections as usize {
            self.connections.total.fetch_sub(1, Ordering::AcqRel);
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "实时连接数已达上限",
            ));
        }

        *per_ip.entry(bucket).or_insert(0) += 1;
        if let Some(key) = subject.as_deref() {
            *per_subject.entry(key.to_string()).or_insert(0) += 1;
        }
        drop(per_subject);
        drop(per_ip);

        Ok(ConnectionGuard {
            connections: self.connections.clone(),
            bucket,
            subject,
        })
    }

    /// 当前连接数。
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.total.load(Ordering::Relaxed)
    }

    /// 当前活跃频道数。
    #[must_use]
    pub fn topic_count(&self) -> usize {
        lock_or_recover(&self.topics, "实时频道表").len()
    }

    /// 是否已关闭。
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// 优雅关闭：**拒绝新连接/新广播**，并让存量订阅者收到通道关闭信号
    /// （WS 发 `Close(1001)`、SSE 结束流）。
    ///
    /// 只负责「发信号」，不等待长连接退出——需要等就用
    /// [`shutdown_and_wait`](Self::shutdown_and_wait)（按 `[realtime].shutdown_timeout`）。
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        // 丢弃 sender → 所有 receiver 立刻收到 `Closed`，长连接自然结束。
        lock_or_recover(&self.topics, "实时频道表").clear();

        // 停掉实例注册心跳：否则关闭后仍会每 5s 往注册表写一次
        #[cfg(all(feature = "queue", feature = "cache"))]
        if let Some(forwarder) = lock_or_recover(&self.forwarder, "实时转发上下文").take() {
            forwarder.stop();
        }
    }

    /// 优雅关闭并**等待长连接退出**（上限 `[realtime].shutdown_timeout`）。
    ///
    /// 应用关闭钩子应优先用这个：`shutdown()` 只是发出关闭信号，进程可能在连接还没
    /// 收尾时就退出，客户端看到的是 `1006` 而不是 `1001`。
    pub async fn shutdown_and_wait(&self) {
        self.shutdown();
        let deadline = tokio::time::Instant::now() + self.config.shutdown_timeout;
        while self.connection_count() > 0 {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    remaining = self.connection_count(),
                    timeout = ?self.config.shutdown_timeout,
                    "实时长连接未在 shutdown_timeout 内退出"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// 淘汰一个**空闲**频道（无订阅者且最久未用）。
fn evict_idle(topics: &mut HashMap<String, TopicEntry>) {
    let victim = topics
        .iter()
        .filter(|(_, entry)| entry.sender.receiver_count() == 0)
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(name, _)| name.clone());
    if let Some(name) = victim {
        topics.remove(&name);
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// 频道订阅句柄：**drop 即注销**。
///
/// 通过 `Deref` 直接当 `broadcast::Receiver` 用（`subscription.recv().await`）。
pub struct Subscription {
    /// `Option` 只为在 `Drop` 里先取出并释放（否则 `receiver_count()` 永远包含自己）。
    receiver: Option<broadcast::Receiver<Arc<RealtimeMessage>>>,
    topics: Weak<Mutex<HashMap<String, TopicEntry>>>,
    topic: String,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("topic", &self.topic)
            .field("alive", &self.receiver.is_some())
            .finish()
    }
}

impl Deref for Subscription {
    type Target = broadcast::Receiver<Arc<RealtimeMessage>>;

    fn deref(&self) -> &Self::Target {
        self.receiver
            .as_ref()
            .expect("Subscription 的 receiver 只在 Drop 时取走")
    }
}

impl DerefMut for Subscription {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.receiver
            .as_mut()
            .expect("Subscription 的 receiver 只在 Drop 时取走")
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // 先释放自己这条 receiver，`receiver_count()` 才可能归零
        drop(self.receiver.take());

        let Some(topics) = self.topics.upgrade() else {
            return;
        };
        let mut map = lock_or_recover(&topics, "实时频道表");
        // 只在「确实没人了」时摘除：`subscribe` 与新订阅者都在同一把锁下，
        // 不会出现「刚建好就被别人摘掉」的竞态。
        if let Some(entry) = map.get(&self.topic)
            && entry.sender.receiver_count() == 0
        {
            map.remove(&self.topic);
        }
    }
}

/// 「请求里没有 `ConnectInfo`」时提示一次：这类连接现在共用「未知来源」配额。
fn warn_unknown_peer() {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "实时连接缺少 ConnectInfo：来源不可知，所有这类连接共用一份 \
             max_connections_per_ip 配额。生产环境请使用 `boot::serve`"
        );
    });
}

/// 连接计数（全局 + 每 IP + 每主体）。
#[derive(Debug, Default)]
struct Connections {
    total: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    per_subject: Mutex<HashMap<String, usize>>,
}

/// 连接名额守卫：**drop 即归还**。
#[derive(Debug)]
pub struct ConnectionGuard {
    connections: Arc<Connections>,
    bucket: IpAddr,
    subject: Option<String>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.connections.total.fetch_sub(1, Ordering::AcqRel);
        // 与 `acquire_connection_for` 保持同一加锁顺序（per_ip → per_subject）
        let mut per_ip = lock_or_recover(&self.connections.per_ip, "实时连接计数");
        if let Some(count) = per_ip.get_mut(&self.bucket) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                per_ip.remove(&self.bucket);
            }
        }
        drop(per_ip);

        if let Some(subject) = self.subject.as_deref() {
            let mut per_subject =
                lock_or_recover(&self.connections.per_subject, "实时主体连接计数");
            if let Some(count) = per_subject.get_mut(subject) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    per_subject.remove(subject);
                }
            }
        }
    }
}

/// 主体标识的计数键：截断到 128 字符，避免超长 `sub` 把计数表撑大。
fn clamp_subject(subject: &str) -> String {
    subject.chars().take(128).collect()
}

/// 从请求扩展里取认证主体（`auth` feature 未启用时恒为 `None`）。
///
/// 认证中间件在**所有路由**上都跑（见 `http::middleware::stack`），所以实时端点
/// 拿到的是已验签的主体——不是客户端自报的字段。
#[must_use]
pub fn subject_from_extensions(extensions: &Extensions) -> Option<String> {
    #[cfg(feature = "auth")]
    {
        extensions
            .get::<crate::auth::Identity>()
            .map(|identity| clamp_subject(&identity.principal.sub))
    }
    #[cfg(not(feature = "auth"))]
    {
        let _ = extensions;
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    #[cfg(feature = "ws")]
    use crate::realtime::message::MessageKind;

    fn hub(max: u32, per_ip: u32) -> Hub {
        Hub::from_config(&RealtimeSection {
            max_connections: max,
            max_connections_per_ip: per_ip,
            ..RealtimeSection::default()
        })
    }

    #[tokio::test]
    async fn should_broadcast_to_subscribers() {
        let hub = hub(10, 10);
        let mut receiver = hub.subscribe("room:1").unwrap();

        let delivered = hub
            .publish("room:1", RealtimeMessage::text("hello"))
            .unwrap();
        assert_eq!(delivered, 1, "应投递给 1 个订阅者");

        let message = receiver.recv().await.unwrap();
        assert_eq!(message.payload, "hello");
        assert_eq!(message.topic, "room:1", "publish 应回填 topic");
    }

    #[tokio::test]
    async fn should_not_deliver_across_topics() {
        let hub = hub(10, 10);
        let mut receiver = hub.subscribe("room:1").unwrap();
        hub.publish("room:2", RealtimeMessage::text("other"))
            .unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), receiver.recv())
                .await
                .is_err(),
            "不同频道不应收到消息"
        );
    }

    #[tokio::test]
    async fn should_return_zero_when_no_subscriber() {
        let hub = hub(10, 10);
        assert_eq!(hub.publish("empty", RealtimeMessage::text("x")).unwrap(), 0);
    }

    #[tokio::test]
    async fn should_not_create_topic_on_publish() {
        // 发布不再创建频道：否则每个被发布过的频道都会永久占一条 256 槽缓冲
        let hub = hub(10, 10);
        hub.publish("ghost", RealtimeMessage::text("x")).unwrap();
        assert_eq!(hub.topic_count(), 0, "无订阅者的发布不应留下频道条目");
    }

    #[tokio::test]
    async fn should_reclaim_topic_when_last_subscriber_leaves() {
        let hub = hub(10, 10);
        let first = hub.subscribe("room:1").unwrap();
        let second = hub.subscribe("room:1").unwrap();
        assert_eq!(hub.topic_count(), 1);

        drop(first);
        assert_eq!(hub.topic_count(), 1, "还有订阅者时不应摘除");

        drop(second);
        assert_eq!(hub.topic_count(), 0, "最后一个订阅者离开应摘除频道");
    }

    #[tokio::test]
    async fn should_cap_topic_count() {
        let hub = Hub::from_config(&RealtimeSection {
            max_topics: 2,
            ..RealtimeSection::default()
        });

        let first = hub.subscribe("a").unwrap();
        let second = hub.subscribe("b").unwrap();
        // 两个频道都有订阅者 → 无空闲频道可淘汰
        assert!(hub.subscribe("c").is_err(), "达上限应拒绝");
        assert_eq!(hub.topic_count(), 2);

        // 释放一个 → 可以淘汰后接纳新频道
        drop(first);
        let third = hub.subscribe("c").unwrap();
        assert_eq!(hub.topic_count(), 2);
        drop(second);
        drop(third);
    }

    #[tokio::test]
    async fn should_reject_oversized_payload() {
        let hub = hub(10, 10);
        let _subscription = hub.subscribe("room:1").unwrap();
        let limit = hub.config().max_message_size_bytes();

        let ok = RealtimeMessage::text("a".repeat(limit));
        assert!(hub.publish("room:1", ok).is_ok(), "恰好等于上限应允许");

        let too_big = RealtimeMessage::text("a".repeat(limit + 1));
        let error = hub.publish("room:1", too_big).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn should_reject_invalid_topic() {
        let hub = hub(10, 10);
        let error = hub.subscribe("room\r\nfake").unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
        assert_eq!(
            hub.publish("room\r\nfake", RealtimeMessage::text("x"))
                .unwrap_err()
                .code(),
            ErrorCode::INVALID_PARAM
        );
    }

    #[test]
    fn should_fail_closed_without_authorizer() {
        let hub = hub(10, 10);
        let headers = HeaderMap::new();
        let extensions = Extensions::new();
        let request = SubscribeRequest {
            topic: "room:1",
            peer: None,
            headers: &headers,
            extensions: &extensions,
        };
        assert!(
            hub.authorize_subscribe(&request).is_err(),
            "未注册钩子必须拒绝"
        );
    }

    #[test]
    fn should_authorize_with_registered_hook() {
        let hub = hub(10, 10);
        hub.on_subscribe(|req| req.topic.starts_with("public:"));
        let headers = HeaderMap::new();
        let extensions = Extensions::new();

        let allowed = SubscribeRequest {
            topic: "public:news",
            peer: None,
            headers: &headers,
            extensions: &extensions,
        };
        assert!(hub.authorize_subscribe(&allowed).is_ok());

        let denied = SubscribeRequest {
            topic: "private:1",
            peer: None,
            headers: &headers,
            extensions: &extensions,
        };
        assert_eq!(
            hub.authorize_subscribe(&denied).unwrap_err().code(),
            ErrorCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn should_enforce_total_connection_limit() {
        let hub = hub(2, 100);
        let first = hub.acquire_connection(None).unwrap();
        let second = hub.acquire_connection(None).unwrap();
        assert_eq!(hub.connection_count(), 2);

        let error = hub.acquire_connection(None).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SERVICE_UNAVAILABLE);

        drop(first);
        assert_eq!(hub.connection_count(), 1, "drop 应归还名额");
        drop(second);
        assert_eq!(hub.connection_count(), 0);
    }

    #[tokio::test]
    async fn should_enforce_per_ip_limit() {
        let hub = hub(100, 1);
        let ip: IpAddr = "203.0.113.7".parse().unwrap();

        let _guard = hub.acquire_connection(Some(ip)).unwrap();
        assert!(hub.acquire_connection(Some(ip)).is_err(), "同 IP 应受限");
        assert!(
            hub.acquire_connection(Some("203.0.113.8".parse().unwrap()))
                .is_ok(),
            "其他 IP 不受影响"
        );
    }

    #[tokio::test]
    async fn should_enforce_per_subject_limit() {
        // 每 IP 限制挡不住「一个账号从大量 IP 开连接」，这里按主体再加一道
        let hub = Hub::from_config(&RealtimeSection {
            max_connections_per_subject: 1,
            ..RealtimeSection::default()
        });
        let first_ip: IpAddr = "203.0.113.1".parse().unwrap();
        let second_ip: IpAddr = "203.0.113.2".parse().unwrap();

        let _guard = hub
            .acquire_connection_for(Some(first_ip), Some("user-1"))
            .unwrap();
        assert!(
            hub.acquire_connection_for(Some(second_ip), Some("user-1"))
                .is_err(),
            "同一主体换 IP 仍应受限"
        );
        assert!(
            hub.acquire_connection_for(Some(second_ip), Some("user-2"))
                .is_ok(),
            "其他主体不受影响"
        );
        // 匿名连接不参与主体维度
        assert!(hub.acquire_connection_for(Some(first_ip), None).is_ok());
    }

    #[tokio::test]
    async fn should_share_quota_when_peer_unknown() {
        // 来源不可知不再「跳过每 IP 限制」：所有未知来源共用一份配额
        let hub = hub(100, 1);
        let _first = hub.acquire_connection(None).unwrap();
        assert!(
            hub.acquire_connection(None).is_err(),
            "未知来源应共用配额，而不是各自不受限"
        );
    }

    #[tokio::test]
    async fn should_close_subscribers_on_shutdown() {
        let hub = hub(10, 10);
        let mut receiver = hub.subscribe("room:1").unwrap();

        hub.shutdown();
        assert!(hub.is_closed());
        assert!(hub.publish("room:1", RealtimeMessage::text("x")).is_err());
        assert!(hub.subscribe("room:1").is_err());
        assert!(hub.acquire_connection(None).is_err());

        assert!(
            matches!(
                receiver.recv().await,
                Err(broadcast::error::RecvError::Closed)
            ),
            "存量订阅者应收到 Closed"
        );
    }

    #[tokio::test]
    async fn should_wait_for_connections_on_shutdown() {
        let hub = Hub::from_config(&RealtimeSection {
            shutdown_timeout: Duration::from_millis(200),
            ..RealtimeSection::default()
        });
        let guard = hub.acquire_connection(None).unwrap();

        let waiter = tokio::spawn({
            let hub = hub.clone();
            async move { hub.shutdown_and_wait().await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(guard);

        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("shutdown_and_wait 应在连接归零后返回")
            .expect("task");
        assert_eq!(hub.connection_count(), 0);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn should_dispatch_inbound_to_handler() {
        let hub = hub(10, 10);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);

        hub.on_message(move |inbound| {
            sink.lock().unwrap().push((
                inbound.topic.clone(),
                inbound.as_str().unwrap_or_default().to_string(),
            ));
        });

        hub.dispatch(InboundMessage::new(
            "room:1",
            MessageKind::Text,
            b"hi".to_vec(),
        ));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "room:1");
        assert_eq!(seen[0].1, "hi");
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn should_ignore_inbound_without_handler() {
        // 未注册处理器时不应 panic，也不影响连接（入站只更新心跳）
        let hub = hub(10, 10);
        hub.dispatch(InboundMessage::new(
            "room:1",
            MessageKind::Text,
            b"hi".to_vec(),
        ));
    }

    #[cfg(all(feature = "queue", feature = "cache"))]
    #[tokio::test]
    async fn should_forward_between_instances_via_queue() {
        use crate::cache::CacheService;
        use crate::config::{CacheSection, QueueSection};
        use crate::queue::QueueService;

        let config = RealtimeSection {
            forward_via_queue: true,
            ..RealtimeSection::default()
        };

        // 两个实例共用同一套后端（模拟两台机器连同一个 Redis/Kafka）
        let queue = QueueService::from_config(&QueueSection::default())
            .await
            .expect("memory queue");
        let cache = CacheService::from_config(&CacheSection::default())
            .await
            .expect("memory cache");

        // 注意：必须**共享同一个** CacheService 实例——内存后端的 `from_config`
        // 每次都会新建独立缓存，那样注册表就不共享了（生产里共用 Redis 不存在该问题）
        let node_a = Hub::from_config(&config).with_forwarding(queue.clone(), cache.clone());
        let node_b = Hub::from_config(&config).with_forwarding(queue.clone(), cache.clone());

        // 等首个心跳把两个实例写进注册表（interval 的首个 tick 立即触发）
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_ne!(node_a.instance_id(), node_b.instance_id());

        let mut subscriber = node_b.subscribe("room:1").unwrap();
        node_a
            .publish("room:1", RealtimeMessage::text("hello from A"))
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(3), subscriber.recv()).await;
        let message = received
            .expect("B 实例应在超时内收到转发消息")
            .expect("recv");
        assert_eq!(message.payload, "hello from A");

        node_a.shutdown();
        node_b.shutdown();
    }
}
