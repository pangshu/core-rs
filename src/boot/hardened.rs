//! 自建 accept loop（feature `serve-hardened`）。
//!
//! ## 为什么需要它
//!
//! `axum::serve` 内部是 `auto::Builder::new(TokioExecutor::new())`，**没有**调用
//! `.timer(...)`。而 hyper 的 `header_read_timeout` 默认值（30s）只在装了 `Timer`
//! 时生效，否则被静默丢弃（`hyper::common::time::Time::Empty` 返回 `None`）。
//!
//! 结果是**没有请求头读超时**：客户端可以一个字节一个字节地发请求头（Slowloris），
//! 连接一直挂着，`[server].request_timeout` 也拦不住——它包的是 service，而 service
//! 要等请求头**读完**才会被调用。
//!
//! 有前置反向代理时这不是问题（代理的 `client_header_timeout` 会先掐断）。所以本模块
//! 是**可选**的：只在「直接暴露公网、没有代理」时打开。
//!
//! ## 与 `axum::serve` 的差异
//!
//! | 能力 | `axum::serve` | 本模块 |
//! |---|---|---|
//! | 请求头读超时 | ❌ 不生效 | ✅ `[server].header_read_timeout` |
//! | `ConnectInfo` | ✅ 自动注入 | ✅ 手动注入（等价） |
//! | 优雅关闭 | ✅ 等全部连接 | ✅ 等全部连接（`JoinSet`） |
//! | 连接生命周期 | 库内维护 | **自己维护**（这是本 feature 的代价） |
//!
//! ## 已知边界
//!
//! - 不做「最大并发连接数」限制：那需要额外配置与拒绝策略（返回 503 还是直接断开），
//!   不在本 feature 范围内。需要时由代理层做。
//! - `accept` 出错（如 fd 耗尽）只记 `warn` 并继续循环，不退出进程——单次
//!   `EMFILE` 不应该让服务整体挂掉，但会持续打日志，便于发现。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tower::ServiceExt;

use crate::boot::StartupError;
use crate::http::AppRouter;

/// 请求头读取超时。
///
/// 取 hyper 自己的默认值（30s），理由：hyper 选这个数是经过权衡的，我们没必要
/// 另立一套。**要更短就靠前置代理的 `client_header_timeout`**——那是按路由/按
/// 站点调的地方，比在框架里加一个全局配置更合理。
pub(super) const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 自建 accept loop 提供服务，`shutdown` 完成时停止 accept 并等在途连接结束。
pub(super) async fn serve_listener_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    app: AppRouter,
    shutdown: F,
) -> Result<(), StartupError>
where
    F: Future<Output = ()> + Send + 'static,
{
    // 关键的一行：装上 `Timer`，`header_read_timeout` 才会真正生效。
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http1().timer(TokioTimer::new());
    builder.http1().header_read_timeout(HEADER_READ_TIMEOUT);
    let builder = Arc::new(builder);

    tracing::info!(
        header_read_timeout = ?HEADER_READ_TIMEOUT,
        "serve-hardened：已启用自建 accept loop（请求头读超时生效）"
    );

    let mut shutdown = std::pin::pin!(shutdown);
    let mut conns = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        // 单次 accept 失败（EMFILE / ECONNABORTED）不该让进程退出
                        tracing::warn!(%error, "accept 失败，继续监听");
                        continue;
                    }
                };
                let app = app.clone();
                let builder = Arc::clone(&builder);
                conns.spawn(async move {
                    // 手写 `ConnectInfo` 注入：`ClientIp`（限流 / IP 白名单 / geoip）
                    // 依赖它；缺了会按 fail-closed 当成「来源不可知」。
                    let service = service_fn(move |mut request: hyper::Request<Incoming>| {
                        let app = app.clone();
                        async move {
                            request.extensions_mut().insert(ConnectInfo(peer));
                            app.oneshot(request.map(Body::new)).await
                        }
                    });
                    if let Err(error) = builder
                        .serve_connection_with_upgrades(TokioIo::new(stream), service)
                        .await
                    {
                        // 对端直接断开很常见，不是错误级别
                        tracing::debug!(%peer, %error, "连接结束");
                    }
                });
            }
            // 回收已完成的任务，避免 JoinSet 无限增长
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }

    // 优雅关闭：不再 accept，等在途连接自己结束
    tracing::info!(
        inflight = conns.len(),
        "serve-hardened：停止 accept，等待在途连接结束"
    );
    while conns.join_next().await.is_some() {}

    Ok(())
}
