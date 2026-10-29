//! cache_and_lock —— M5 示例：缓存读写 + 分布式锁。
//!
//! 运行：`cargo run --example cache_and_lock --features cache`
//!
//! 演示：
//! - `get_or_set`：未命中回源并回填（内置 singleflight / TTL 抖动 / 空值缓存）
//! - `lock`：分布式锁，**drop 自动释放**（Redis 后端为 `SET NX PX` + Lua compare-and-del）
//! - 缓存后端由 `[cache].backend` 决定（`memory` / `redis`）
//!
//! 配置（`config/default.toml` 的 `[cache]`）：
//! `singleflight.enabled` / `ttl_jitter` / `null_cache_ttl` / `lock.default_ttl`。

use std::time::Duration;

use aurora::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = preset::load_env_var("AURORA_ENV", "config")?;
    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    // 1) 读缓存：未命中则回源（模拟查库）并回填
    let value = state
        .cache
        .get_or_set("user:1", Some(Duration::from_secs(60)), || async {
            tracing::info!("缓存未命中，回源（模拟查库）");
            Ok(Some(br#"{"id":1,"name":"alice"}"#.to_vec()))
        })
        .await?;
    tracing::info!(value = ?value.as_deref().map(String::from_utf8_lossy), "第一次读取");

    // 2) 再读一次：应直接命中缓存，不再回源
    let cached = state
        .cache
        .get_or_set("user:1", Some(Duration::from_secs(60)), || async {
            tracing::info!("不应出现：本应命中缓存");
            Ok(None)
        })
        .await?;
    tracing::info!(hit = cached.is_some(), "第二次读取");

    // 3) 分布式锁：拿到锁进入临界区，guard drop 即释放
    match state.cache.lock("order:123", None).await? {
        Some(guard) => {
            tracing::info!(key = guard.key(), "获得锁，进入临界区");
            // 持锁期间同一键不可再获取
            let reentrant = state.cache.lock("order:123", None).await?;
            tracing::info!(
                reentrant = reentrant.is_some(),
                "持锁期间再次获取应为 false"
            );
        }
        None => tracing::warn!("锁已被其他实例占用"),
    }

    // 4) 释放后可再次获取
    let again = state.cache.lock("order:123", None).await?;
    tracing::info!(acquired = again.is_some(), "drop 后应可再次获取");

    state.cache.del("user:1").await?;
    tracing::info!("示例结束");
    Ok(())
}
