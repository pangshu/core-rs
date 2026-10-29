//! Redis 连接的共用参数。
//!
//! `redis::aio::ConnectionManager` 的两个超时默认都是 `None`，会在两处咬人：
//!
//! 1. **建连**（`connection_timeout`）：连不上时一直等**操作系统**的 TCP 超时
//!    （Windows 上实测可达数分钟），启动期表现为「卡住不动」。
//! 2. **命令响应**（`response_timeout`）：连接中途断掉时，在途命令**永不返回**——
//!    实测 `get_or_set` 挂住 >10s，降级逻辑根本没机会执行。
//!
//! 所以统一走 [`connect`]：它用 [`ConnectionManagerConfig`] 同时设好两个上限。
//! 注意 `connection_timeout` 对**每次重连**都生效，比在调用点包一层
//! `tokio::time::timeout` 更彻底（后者只覆盖首次建连）。
//!
//! 覆盖的后端：`cache-redis` / `queue-redis` / `ratelimit-redis` / `session-redis`。

use std::time::Duration;

use redis::aio::{ConnectionManager, ConnectionManagerConfig};

/// 建立 Redis 连接的超时上限（对每次重连也生效）。
///
/// 取 5 秒：正常的同机房 Redis 建连在毫秒级，5 秒足够区分「慢」与「不可达」。
/// 超过就该在启动期报错，而不是让进程挂在那里。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 单条命令的响应超时。
///
/// 取 3 秒：缓存/队列/限流用到的都是小命令（`GET` / `SET` / `RPOPLPUSH` / 短 Lua），
/// 正常在毫秒级。设上限的意义是**让「连接假死」变成可恢复的错误**——
/// 否则一次网络分区就会把请求 handler 全部拖住，比直接报错更糟。
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

/// 按统一参数建立连接管理器。
///
/// # Errors
/// URL 非法、建连失败或超过 [`CONNECT_TIMEOUT`] 时返回 `RedisError`。
pub(crate) async fn connect(url: &str) -> redis::RedisResult<ConnectionManager> {
    let client = redis::Client::open(url)?;
    let config = ConnectionManagerConfig::new()
        .set_connection_timeout(CONNECT_TIMEOUT)
        .set_response_timeout(RESPONSE_TIMEOUT)
        // 默认重试 6 次、指数退避（100ms 起、无上限）——对「后端已挂」这个场景是错的：
        // 一条读命令会被拖着重连好几轮，实测挂住 >10s，降级逻辑根本没机会执行。
        // 重试 1 次足够覆盖瞬时抖动，剩下交给上层降级（缓存）或快速失败（队列/限流）。
        .set_number_of_retries(1)
        .set_max_delay(500);
    client.get_connection_manager_with_config(config).await
}
