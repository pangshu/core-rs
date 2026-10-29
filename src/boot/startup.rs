//! 启动状态机与启动错误。
//!
//! 应用启动是一条**有序状态机**，任一步失败即**逆序回滚 + 非零退出**
//! （启动期无 HTTP，错误类型为 [`StartupError`]，**不映射业务错误码**）。
//!
//! 步骤（见 `docs/02-architecture.md` §13）：
//! 1. 加载并校验配置 → `StartupError::Config`
//! 2. 初始化 telemetry → `StartupError::Telemetry`
//! 3. 连接资源并 ping（M2+）
//! 4. 执行迁移（M2+）
//! 5. 组合一致性校验（M2+）
//! 6. 构建 `AppState`
//! 7. 构建 Router
//! 8. 启动后台任务（M2+）
//! 9. 绑定 `TcpListener` → `StartupError::Bind`
//! 10. 置位 `ready`
//! 11. `axum::serve` → `StartupError::Serve`

/// 启动期错误。
///
/// 独立于 [`crate::errors::AppError`]：启动期不映射 HTTP 状态码，
/// 经 `?` 汇入应用层 `anyhow` 后以非零码退出。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartupError {
    /// 配置加载或校验失败（状态机第 1 步）。
    #[error("config error: {0}")]
    Config(String),

    /// telemetry 初始化失败（状态机第 2 步）。
    #[error("telemetry error: {0}")]
    Telemetry(String),

    /// 资源连接失败（状态机第 3 步）。
    #[error("resource error: {0}")]
    Resource(String),

    /// 数据库迁移失败（状态机第 4 步）。
    #[error("migration error: {0}")]
    Migration(String),

    /// 组合一致性校验失败（状态机第 5 步）。
    #[error("combination error: {0}")]
    Combination(String),

    /// 监听地址绑定失败（状态机第 9 步）。
    #[error("bind error: {0}")]
    Bind(String),

    /// 服务运行期错误（状态机第 11 步）。
    #[error("serve error: {0}")]
    Serve(String),
}

/// 启动期提示：应用层缺少「请求头读超时」，需由前置代理承担。
///
/// 背景（已核实到依赖源码）：
/// - `hyper` 1.12 的 `http1::Builder` 有 30 秒的 `header_read_timeout` 默认值；
/// - 但该默认值只在**设置了 `Timer`** 时生效，否则被丢弃（`hyper::common::time`
///   对 `Time::Empty` 返回 `None`）；
/// - `axum::serve` 用的是 `auto::Builder::new(TokioExecutor::new())`，**没有**调用
///   `.timer(...)`，因此上述默认值不生效。
///
/// 结果是：请求头可以无限慢地发送（Slowloris），而 `[server].request_timeout`
/// 拦不住——`TimeoutLayer` 包的是 service，service 要等请求头**读完**才会被调用。
///
/// 当前正确解法是前置反向代理配置 header 超时；这里在启动期把它喊出来，
/// 避免"以为配了超时就安全了"。
pub(crate) fn warn_transport_hardening(config: &crate::config::AppConfig) {
    // 启用 `serve-hardened` 时请求头读超时已经生效，这条告警不成立。
    #[cfg(feature = "serve-hardened")]
    {
        let _ = config;
        return;
    }

    // 只对公网可达的监听地址告警：127.0.0.1 / ::1 这类本机绑定不在攻击面内。
    #[cfg(not(feature = "serve-hardened"))]
    {
        let public_bind = matches!(config.server.host.trim(), "0.0.0.0" | "::" | "[::]" | "::0");
        if !public_bind {
            return;
        }
        // 每个进程只喊一次（测试里会反复构建 AppState，不该刷屏）
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                host = %config.server.host,
                port = config.server.port,
                request_timeout = ?config.server.request_timeout,
                "应用层不提供请求头读超时（axum::serve 未安装 Timer，hyper 的 30s 默认值不生效）：\
                 请确认前置反向代理已配置 client_header_timeout（nginx）/ request_headers_timeout（envoy），\
                 否则存在 Slowloris 风险。注意 [server].request_timeout 只覆盖「请求头读完之后」的阶段。\
                 若服务直接暴露公网、没有前置代理，请启用 feature `serve-hardened`"
            );
        });
    }
}
