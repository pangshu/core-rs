//! HTTPS 服务启动（`axum-server` + rustls + SNI）。
//!
//! 与 [`crate::boot::serve`] 的区别：TLS 由 `axum_server` 承担，优雅关闭改用其
//! [`Handle`](axum_server::Handle) 通知；`ConnectInfo` 仍照常注入，因此
//! `ClientIp` / 限流 / geoip 的行为与 HTTP 一致。

use std::net::SocketAddr;
use std::time::Duration;

use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};

use crate::boot::StartupError;
use crate::boot::shutdown_signal;
use crate::http::router::AppRouter;
use crate::tls::TlsService;

/// 优雅关闭等待上限（与 `[server].graceful_shutdown_timeout` 默认值一致）。
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// 绑定地址并以 HTTPS 启动服务，收到 `SIGINT` / `SIGTERM` 时优雅关闭。
///
/// 握手超时由 `[tls].handshake_timeout` 决定（默认 10s）：`axum-server` 自带
/// 10s 硬编码，这里显式传一次，让「慢速 ClientHello 占满 fd」的窗口可运维调节。
///
/// ```no_run
/// # async fn run(app: aurora::http::AppRouter, cfg: aurora::config::AppConfig) -> Result<(), Box<dyn std::error::Error>> {
/// use aurora::prelude::*;
///
/// let tls = TlsService::from_config(&cfg.tls)?;
/// serve_tls(app, cfg.server.socket_addr()?, tls).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
/// 绑定失败或运行期错误返回 [`StartupError`]。
pub async fn serve_tls(
    app: AppRouter,
    addr: SocketAddr,
    service: TlsService,
) -> Result<(), StartupError> {
    let config = RustlsConfig::from_config(service.server_config());
    let acceptor =
        RustlsAcceptor::new(config.clone()).handshake_timeout(service.handshake_timeout());
    let handle = axum_server::Handle::new();

    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("收到关闭信号，开始优雅关闭（TLS）");
        shutdown_handle.graceful_shutdown(Some(GRACEFUL_SHUTDOWN_TIMEOUT));
    });

    tracing::info!(%addr, "aurora listening (https)");
    axum_server::bind_rustls(addr, config)
        .acceptor(acceptor)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(|error| StartupError::Serve(error.to_string()))?;

    crate::telemetry::shutdown();
    Ok(())
}
