//! 优雅关闭信号。
//!
//! 监听 `SIGINT`（Windows 为 `Ctrl-C`）与 `SIGTERM`（Unix）；收到后停止接收
//! 新连接并等待在途请求完成（关闭顺序见 `docs/02-architecture.md` §3）。

/// 等待关闭信号。
///
/// 在 `SIGINT` 或 `SIGTERM`（Unix）到达时返回。
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => tracing::error!(error = %e, "failed to listen for SIGTERM"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
