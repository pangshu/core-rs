//! Redis 队列后端（`queue-redis`）。
//!
//! 结构（前缀 `aurora:queue:{queue}`）：
//!
//! | 键 | 类型 | 用途 |
//! |---|---|---|
//! | `{p}:ready` | LIST | 待消费（`LPUSH` 入队、`RPOP` 出队） |
//! | `{p}:inflight` | LIST | 已出队未 ack（`RPOPLPUSH` 原子移入） |
//! | `{p}:inflight_at` | ZSET | `id → 出队时间（Redis 服务端毫秒）`，用于可见性超时回收 |
//! | `{p}:payloads` | HASH | `id → 消息 JSON`（出队后按 id 取回） |
//! | `{p}:dead` | LIST | 死信（消息 JSON） |
//! | `{p}:dead_replaying` | LIST | 重放中的死信（`RPOPLPUSH` 的临时落点，崩溃后可续） |
//!
//! **为什么 ready 只存 id**：`ack` 只拿到 `id`，而 `LREM` 按**元素值**匹配——
//! 若 inflight 存的是整条消息就无法按 id 移除。因此 ready/inflight 存 id，
//! 消息体放 HASH，ack 时 `LREM` + `ZREM` + `HDEL` 在**一段 Lua 里原子完成**。
//!
//! **崩溃语义**：`RPOPLPUSH` 原子完成「出队 + 进 inflight」，进程崩溃后消息仍在
//! `inflight` 里。`{p}:inflight_at` 记录出队时间，超过 `[queue].visibility_timeout`
//! 仍未 ack 的消息由 [`RedisQueue::reap_inflight`] 移回 ready 重投
//! （`dequeue` 内按队列 1 秒一次的机会式触发，无需外部定时器）。
//!
//! **死信重放**：先 `RPOPLPUSH dead → dead_replaying` 原子取走，再落回 ready；
//! 中途崩溃的条目留在 `dead_replaying`，下次重放会先把它续完，不会丢。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::queue::{Message, QueueBackend};

/// 键前缀。
const PREFIX: &str = "aurora:queue";

/// 死信列表上限（`LPUSH` 后 `LTRIM`，保留最新的 N 条）。
///
/// 不设上限时，一个死循环的重试会把 Redis 内存吃光；代价是最旧的死信被丢弃，
/// 因此重放/巡检要及时。
const MAX_DEAD: usize = 10_000;

/// 单次回收的最大条数（避免一次 Lua 卡住 Redis）。
const REAP_BATCH: usize = 64;

/// `dead_letters` 单次返回上限（超出会 `warn`，不静默截断）。
const DEAD_PEEK_LIMIT: usize = 100;

/// 同一队列的回收节流间隔。
const REAP_INTERVAL: Duration = Duration::from_secs(1);

/// 入队：写消息体 + 推 id。
const ENQUEUE_LUA: &str = r"
redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
redis.call('LPUSH', KEYS[1], ARGV[1])
return 1
";

/// 出队：原子「ready → inflight」，打上出队时间，再按 id 取消息体。
///
/// 取不到消息体说明 HASH 已被清理（异常状态），顺手把 id 从 inflight 摘掉。
const DEQUEUE_LUA: &str = r"
local id = redis.call('RPOPLPUSH', KEYS[1], KEYS[2])
if not id then
    return nil
end
local payload = redis.call('HGET', KEYS[3], id)
if not payload then
    redis.call('LREM', KEYS[2], 1, id)
    return nil
end
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZADD', KEYS[4], now, id)
return {id, payload}
";

/// 确认：移出 inflight + 清出队时间 + 删消息体。
const ACK_LUA: &str = r"
redis.call('LREM', KEYS[1], 1, ARGV[1])
redis.call('ZREM', KEYS[3], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
return 1
";

/// 重投：写回消息体（attempts 已递增）+ 移出 inflight + 重新入队。
const REQUEUE_LUA: &str = r"
redis.call('HSET', KEYS[3], ARGV[1], ARGV[2])
redis.call('LREM', KEYS[1], 1, ARGV[1])
redis.call('ZREM', KEYS[4], ARGV[1])
redis.call('LPUSH', KEYS[2], ARGV[1])
return 1
";

/// 死信：移出 inflight + 清出队时间 + 推入 dead（并裁剪）+ 删消息体。
const DEAD_LUA: &str = r"
redis.call('LREM', KEYS[1], 1, ARGV[1])
redis.call('ZREM', KEYS[4], ARGV[1])
redis.call('LPUSH', KEYS[2], ARGV[2])
redis.call('LTRIM', KEYS[2], 0, tonumber(ARGV[3]) - 1)
redis.call('HDEL', KEYS[3], ARGV[1])
return 1
";

/// 回收：把超过可见性超时的 in-flight id 移回 ready。
///
/// 用 Redis 的 `TIME` 而不是调用方传入的绝对时间，避免客户端时钟漂移
/// 把「刚出队」的消息判成超时。
const REAP_LUA: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
local cutoff = now - tonumber(ARGV[1])
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', cutoff, 'LIMIT', 0, tonumber(ARGV[2]))
local moved = 0
for i = 1, #ids do
    local id = ids[i]
    redis.call('ZREM', KEYS[1], id)
    redis.call('LREM', KEYS[2], 1, id)
    if redis.call('HGET', KEYS[3], id) then
        redis.call('LPUSH', KEYS[4], id)
        moved = moved + 1
    end
end
return moved
";

/// Redis 队列后端。
#[derive(Clone)]
pub struct RedisQueue {
    conn: ConnectionManager,
    /// in-flight 可见性超时（`[queue].visibility_timeout`）。
    visibility_timeout: Duration,
    /// 每队列上次回收时间（把回收节流到 [`REAP_INTERVAL`]）。
    last_reap: Arc<Mutex<HashMap<String, Instant>>>,
}

impl std::fmt::Debug for RedisQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisQueue").finish_non_exhaustive()
    }
}

impl RedisQueue {
    /// 按连接串建立连接（可见性超时默认 `60s`）。
    ///
    /// # Errors
    /// URL 非法或连接失败时返回 `50003`。
    pub async fn connect(url: &str) -> AppResult<Self> {
        // 建连超时 + 命令响应超时都在 `utils::redis::connect` 里设好
        let conn = crate::utils::redis::connect(url).await.map_err(map_err)?;
        Ok(Self {
            conn,
            visibility_timeout: Duration::from_secs(60),
            last_reap: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 设置 in-flight 可见性超时。
    #[must_use]
    pub fn with_visibility_timeout(mut self, timeout: Duration) -> Self {
        self.visibility_timeout = timeout;
        self
    }

    /// 当前可见性超时。
    #[must_use]
    pub fn visibility_timeout(&self) -> Duration {
        self.visibility_timeout
    }

    fn ready(&self, queue: &str) -> String {
        format!("{PREFIX}:{queue}:ready")
    }

    fn inflight(&self, queue: &str) -> String {
        format!("{PREFIX}:{queue}:inflight")
    }

    fn inflight_at(&self, queue: &str) -> String {
        format!("{PREFIX}:{queue}:inflight_at")
    }

    fn payloads(&self, queue: &str) -> String {
        format!("{PREFIX}:{queue}:payloads")
    }

    fn dead(&self, queue: &str) -> String {
        format!("{PREFIX}:{queue}:dead")
    }

    fn dead_replaying(&self, queue: &str) -> String {
        format!("{PREFIX}:{queue}:dead_replaying")
    }

    async fn raw_dead(&self, queue: &str, limit: usize) -> AppResult<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.clone();
        redis::cmd("LRANGE")
            .arg(self.dead(queue))
            .arg(0)
            .arg(limit.saturating_sub(1) as i64)
            .query_async(&mut conn)
            .await
            .map_err(map_err)
    }

    /// 机会式回收：每个队列最多 [`REAP_INTERVAL`] 触发一次。
    ///
    /// 放在 `dequeue` 里而不是起后台任务，是为了不引入「谁来 stop 这个任务」的生命周期
    /// 问题——只要有消费者在出队，回收就一定在跑。
    async fn maybe_reap(&self, queue: &str) {
        if self.visibility_timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        {
            let Ok(mut guard) = self.last_reap.lock() else {
                return;
            };
            match guard.get(queue) {
                Some(previous) if now.duration_since(*previous) < REAP_INTERVAL => return,
                _ => {
                    guard.insert(queue.to_string(), now);
                }
            }
        }
        if let Err(error) = self.reap_inflight(queue, self.visibility_timeout).await {
            tracing::warn!(%error, queue, "回收 in-flight 超时消息失败（下次出队会重试）");
        }
    }

    /// 把超过 `timeout` 仍未 ack 的消息移回 ready，返回回收条数。
    ///
    /// 这些消息可能仍在原消费者手里处理（只是慢了），因此**会重复投递**——
    /// 消费端必须幂等。`timeout` 应大于单条消息的最长处理时间。
    ///
    /// # Errors
    /// Redis 访问失败时返回 `50003`。
    pub async fn reap_inflight(&self, queue: &str, timeout: Duration) -> AppResult<usize> {
        let timeout_ms = i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX);
        let mut conn = self.conn.clone();
        let moved: i64 = redis::Script::new(REAP_LUA)
            .key(self.inflight_at(queue))
            .key(self.inflight(queue))
            .key(self.payloads(queue))
            .key(self.ready(queue))
            .arg(timeout_ms)
            .arg(REAP_BATCH as i64)
            .invoke_async(&mut conn)
            .await
            .map_err(map_err)?;

        if moved > 0 {
            tracing::warn!(
                queue,
                moved,
                timeout_ms,
                "回收 in-flight 超时消息并重新入队（可能重复消费，消费端需幂等）"
            );
        }
        Ok(usize::try_from(moved).unwrap_or(0))
    }

    /// 把一条已取走的死信落回 ready（重放的一步）。
    async fn replay_one(&self, queue: &str, body: &str) -> AppResult<()> {
        let mut message = serde_json::from_str::<Message>(body).map_err(encode_err)?;
        // 重放视为重新投递：不复位 attempts 的话，重放出去立刻又满足「已耗尽」条件，
        // 下一次失败直接回到死信——重放等于没放。
        message.attempts = 0;
        message.queue = queue.to_string();
        self.enqueue(queue, message).await
    }
}

#[async_trait]
impl QueueBackend for RedisQueue {
    async fn enqueue(&self, queue: &str, message: Message) -> AppResult<()> {
        let body = serde_json::to_string(&message).map_err(encode_err)?;
        let mut conn = self.conn.clone();
        redis::Script::new(ENQUEUE_LUA)
            .key(self.ready(queue))
            .key(self.payloads(queue))
            .arg(&message.id)
            .arg(body)
            .invoke_async::<i64>(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn dequeue(&self, queue: &str) -> AppResult<Option<Message>> {
        // 先回收超时的 in-flight，再取新消息：崩溃遗留的消息不会一直躺着。
        self.maybe_reap(queue).await;

        let mut conn = self.conn.clone();
        let got: Option<(String, String)> = redis::Script::new(DEQUEUE_LUA)
            .key(self.ready(queue))
            .key(self.inflight(queue))
            .key(self.payloads(queue))
            .key(self.inflight_at(queue))
            .invoke_async(&mut conn)
            .await
            .map_err(map_err)?;

        let Some((id, body)) = got else {
            return Ok(None);
        };

        match serde_json::from_str::<Message>(&body) {
            Ok(message) => Ok(Some(message)),
            Err(error) => {
                // 消息体损坏：不要卡在 inflight 里，转死信后继续（顺手清掉 inflight 记录）。
                tracing::error!(%error, queue, id, "Redis 队列消息解析失败，转入死信");
                self.dead_letter_raw(queue, &id, &body).await?;
                Ok(None)
            }
        }
    }

    async fn ack(&self, queue: &str, id: &str) -> AppResult<()> {
        let mut conn = self.conn.clone();
        redis::Script::new(ACK_LUA)
            .key(self.inflight(queue))
            .key(self.payloads(queue))
            .key(self.inflight_at(queue))
            .arg(id)
            .invoke_async::<i64>(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn requeue(&self, queue: &str, message: Message) -> AppResult<()> {
        // 重投保留原消息体（attempts 已由上层递增），只把 id 挪回 ready
        let body = serde_json::to_string(&message).map_err(encode_err)?;
        let mut conn = self.conn.clone();
        redis::Script::new(REQUEUE_LUA)
            .key(self.inflight(queue))
            .key(self.ready(queue))
            .key(self.payloads(queue))
            .key(self.inflight_at(queue))
            .arg(&message.id)
            .arg(body)
            .invoke_async::<i64>(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn dead_letter(&self, queue: &str, message: Message) -> AppResult<()> {
        let body = serde_json::to_string(&message).map_err(encode_err)?;
        self.dead_letter_raw(queue, &message.id, &body).await
    }

    async fn len(&self, queue: &str) -> AppResult<usize> {
        let mut conn = self.conn.clone();
        let len: i64 = redis::cmd("LLEN")
            .arg(self.ready(queue))
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(usize::try_from(len).unwrap_or(0))
    }

    async fn dead_letters(&self, queue: &str) -> AppResult<Vec<Message>> {
        let raw = self.raw_dead(queue, DEAD_PEEK_LIMIT).await?;
        if raw.len() >= DEAD_PEEK_LIMIT {
            // 静默截断会让人误以为死信就这么多；重放不受这个上限影响（它按 max 循环取）。
            tracing::warn!(
                queue,
                limit = DEAD_PEEK_LIMIT,
                "死信查询达到返回上限，结果可能被截断"
            );
        }
        Ok(raw
            .iter()
            .filter_map(|body| serde_json::from_str::<Message>(body).ok())
            .collect())
    }

    async fn replay_dead(&self, queue: &str, max: usize) -> AppResult<usize> {
        let mut conn = self.conn.clone();
        let mut replayed = 0usize;

        // 1) 先续上「上次重放到一半」的条目（`dead_replaying` 是崩溃安全网）。
        let leftovers: Vec<String> = redis::cmd("LRANGE")
            .arg(self.dead_replaying(queue))
            .arg(0)
            .arg(-1)
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        for body in leftovers {
            if replayed >= max {
                break;
            }
            self.replay_one(queue, &body).await?;
            let _: i64 = redis::cmd("LREM")
                .arg(self.dead_replaying(queue))
                .arg(1)
                .arg(&body)
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            replayed += 1;
        }

        // 2) 再从 dead 里原子取（`RPOPLPUSH`：取走的同时留一份在 replaying，
        //    中途崩溃不会丢消息）。
        while replayed < max {
            let body: Option<String> = redis::cmd("RPOPLPUSH")
                .arg(self.dead(queue))
                .arg(self.dead_replaying(queue))
                .query_async(&mut conn)
                .await
                .map_err(map_err)?;
            let Some(body) = body else {
                break;
            };

            match self.replay_one(queue, &body).await {
                Ok(()) => {
                    let _: i64 = redis::cmd("LREM")
                        .arg(self.dead_replaying(queue))
                        .arg(1)
                        .arg(&body)
                        .query_async(&mut conn)
                        .await
                        .map_err(map_err)?;
                    replayed += 1;
                }
                Err(error) => {
                    // 放回 dead，别让一条坏消息卡住整个重放。
                    let _: i64 = redis::cmd("LREM")
                        .arg(self.dead_replaying(queue))
                        .arg(1)
                        .arg(&body)
                        .query_async(&mut conn)
                        .await
                        .map_err(map_err)?;
                    let _: i64 = redis::cmd("LPUSH")
                        .arg(self.dead(queue))
                        .arg(&body)
                        .query_async(&mut conn)
                        .await
                        .map_err(map_err)?;
                    return Err(error);
                }
            }
        }

        Ok(replayed)
    }

    async fn ping(&self) -> AppResult<()> {
        let mut conn = self.conn.clone();
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

impl RedisQueue {
    /// 把一条原始消息体推入死信（消息体可能已经无法反序列化）。
    async fn dead_letter_raw(&self, queue: &str, id: &str, body: &str) -> AppResult<()> {
        let mut conn = self.conn.clone();
        redis::Script::new(DEAD_LUA)
            .key(self.inflight(queue))
            .key(self.dead(queue))
            .key(self.payloads(queue))
            .key(self.inflight_at(queue))
            .arg(id)
            .arg(body)
            .arg(MAX_DEAD as i64)
            .invoke_async::<i64>(&mut conn)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

fn map_err(error: redis::RedisError) -> AppError {
    tracing::error!(%error, "Redis 队列访问失败");
    AppError::biz(ErrorCode::UPSTREAM_ERROR, "队列后端不可用")
}

fn encode_err(error: serde_json::Error) -> AppError {
    AppError::biz(
        ErrorCode::UPSTREAM_ERROR,
        format!("队列消息编解码失败：{error}"),
    )
}

/// 供测试观察 inflight 长度（崩溃后残留的消息数）。
///
/// # Errors
/// Redis 访问失败时返回 `50003`。
pub async fn inflight_len(queue: &RedisQueue, name: &str) -> AppResult<usize> {
    let mut conn = queue.conn.clone();
    let len: i64 = redis::cmd("LLEN")
        .arg(queue.inflight(name))
        .query_async(&mut conn)
        .await
        .map_err(map_err)?;
    Ok(usize::try_from(len).unwrap_or(0))
}

/// 键名（供测试断言）。
#[must_use]
pub fn keys(queue: &str) -> (String, String, String, String, String) {
    (
        format!("{PREFIX}:{queue}:ready"),
        format!("{PREFIX}:{queue}:inflight"),
        format!("{PREFIX}:{queue}:payloads"),
        format!("{PREFIX}:{queue}:dead"),
        format!("{PREFIX}:{queue}:inflight_at"),
    )
}
