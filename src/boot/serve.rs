//! 绑定监听并启动服务。

use std::future::Future;
use std::net::SocketAddr;

use crate::boot::shutdown::shutdown_signal;
use crate::boot::startup::StartupError;
use crate::http::router::AppRouter;

/// 绑定地址、启动 axum 服务，并在 `SIGINT` / `SIGTERM` 到达时优雅关闭。
///
/// 对应启动状态机第 9–11 步：绑定 `TcpListener` → 置位就绪 → `axum::serve`
/// 并挂载优雅关闭。
///
/// # Errors
/// 绑定失败返回 [`StartupError::Bind`]；运行期错误返回 [`StartupError::Serve`]。
pub async fn serve(app: AppRouter, addr: SocketAddr) -> Result<(), StartupError> {
    serve_with_shutdown(app, addr, shutdown_signal()).await
}

/// 绑定地址、启动服务，并在给定 `shutdown` future 完成时优雅关闭。
///
/// 便于在测试中注入自定义关闭信号（生产环境请用 [`serve`]）。
///
/// # Errors
/// 绑定失败返回 [`StartupError::Bind`]；运行期错误返回 [`StartupError::Serve`]。
pub async fn serve_with_shutdown<F>(
    app: AppRouter,
    addr: SocketAddr,
    shutdown: F,
) -> Result<(), StartupError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| StartupError::Bind(format!("{addr}: {e}")))?;
    tracing::info!(%addr, "aurora listening");
    serve_listener_with_shutdown(listener, app, shutdown).await
}

/// 在已绑定的 [`tokio::net::TcpListener`] 上启动服务，并在 `shutdown` 完成时优雅关闭。
///
/// # Errors
/// 运行期错误返回 [`StartupError::Serve`]。
pub async fn serve_listener_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    app: AppRouter,
    shutdown: F,
) -> Result<(), StartupError>
where
    F: Future<Output = ()> + Send + 'static,
{
    // feature `serve-hardened`：走自建 accept loop，让请求头读超时真正生效。
    // 默认路径（`axum::serve`）不装 hyper 的 `Timer`，那个 30s 默认值会被丢弃——
    // 详见 `boot::startup::warn_transport_hardening` 的说明。
    #[cfg(feature = "serve-hardened")]
    let result = crate::boot::hardened::serve_listener_with_shutdown(listener, app, shutdown).await;

    // 默认路径：注入 `ConnectInfo<SocketAddr>`，使 `ClientIp`（限流 / 审计 / geoip
    // 的前置）在未信任代理头时也能拿到 TCP 对端地址。
    #[cfg(not(feature = "serve-hardened"))]
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| StartupError::Serve(e.to_string()));

    // 刷新未发送的追踪 span（feature `otel`；未启用时为空操作）
    crate::telemetry::shutdown();
    result
}
