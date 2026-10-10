//! rustls `ServerConfig` 构建与 HTTP → HTTPS 跳转。
//!
//! `ServerConfig` **只构建一次**，内嵌读取 `ArcSwap<CertStore>` 的动态 resolver；
//! 证书热更新只替换仓库，故无需重建 `ServerConfig`、无需重启。

use std::sync::Arc; // 引入 Arc，共享证书仓库与 crypto provider

use axum_server::tls_rustls::RustlsConfig; // 引入 axum-server 的 rustls 配置类型
use rustls::SupportedProtocolVersion; // 引入协议版本类型，用于限定最低版本

use super::store::{CertStore, DynamicResolver}; // 引入证书仓库与动态解析器
use super::TlsError; // 引入 TLS 错误类型

/// 由配置与证书仓库构建 rustls 配置（crypto provider 固定为 ring）
pub fn build_rustls_config( // 构建 rustls 配置
    min_version: &str, // 最低协议版本（"1.2" | "1.3"）
    store: Arc<arc_swap::ArcSwap<CertStore>>, // 证书仓库句柄
) -> Result<RustlsConfig, TlsError> { // 返回 rustls 配置
    let provider = Arc::new(rustls::crypto::ring::default_provider()); // 显式选择 ring provider
    let versions: &[&SupportedProtocolVersion] = if min_version.trim() == "1.3" { // 依据最低版本选择协议集
        &[&rustls::version::TLS13] // 仅 TLS 1.3
    } else {
        &[&rustls::version::TLS12, &rustls::version::TLS13] // TLS 1.2 起（默认）
    };
    let mut config = rustls::ServerConfig::builder_with_provider(provider) // 用 provider 构造 builder
        .with_protocol_versions(versions) // 设定协议版本集
        .map_err(|e| TlsError::Config(e.to_string()))? // 失败转配置错误
        .with_no_client_auth() // v1 不做 mTLS
        .with_cert_resolver(Arc::new(DynamicResolver::new(store))); // 内嵌动态 SNI resolver
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()]; // 声明 ALPN：HTTP/2 与 HTTP/1.1
    Ok(RustlsConfig::from_config(Arc::new(config))) // 包装为 axum-server 配置
}

/// 启动 HTTP → HTTPS 308 跳转监听（可选项，`redirect_http = true` 时调用）。
/// 与主服务共用同一个停机信号（`shutdown` 变更即优雅退出）。
pub fn spawn_http_redirect( // 启动跳转监听
    host: String, // 监听地址
    http_port: u16, // 明文端口
    https_port: u16, // 目标 HTTPS 端口
    mut shutdown: tokio::sync::watch::Receiver<bool>, // 停机信号
) {
    tokio::spawn(async move { // 后台任务
        let app = axum::Router::new() // 新建路由
            .fallback(redirect_handler) // 全部路径走跳转
            .with_state(https_port); // 注入目标端口
        let addr = format!("{host}:{http_port}"); // 拼接监听地址
        let listener = match tokio::net::TcpListener::bind(&addr).await { // 绑定端口
            Ok(l) => l, // 绑定成功
            Err(e) => { // 绑定失败
                tracing::warn!(error = %e, addr = %addr, "http->https redirect listener bind failed"); // 告警（不阻断主服务）
                return; // 放弃跳转监听
            }
        };
        tracing::info!(addr = %addr, "http->https redirect listening"); // 记录已监听
        let _ = axum::serve(listener, app) // 启动跳转服务
            .with_graceful_shutdown(async move { let _ = shutdown.changed().await; }) // 随停机信号退出
            .await; // 等待结束
    });
}

/// 跳转处理：按 Host + 路径拼出 https 目标，返回 308（保留方法语义）
async fn redirect_handler( // 跳转处理函数
    axum::extract::State(https_port): axum::extract::State<u16>, // 目标 HTTPS 端口
    req: axum::extract::Request, // 原请求
) -> axum::response::Response { // 返回响应
    use axum::http::header::LOCATION; // 引入 Location 头名
    use axum::http::StatusCode; // 引入状态码
    use axum::response::IntoResponse; // 引入转响应 trait

    let host = req // 从 Host 头取主机名（去掉端口）
        .headers() // 请求头
        .get(axum::http::header::HOST) // Host 头
        .and_then(|v| v.to_str().ok()) // 转为字符串
        .map(|h| h.split(':').next().unwrap_or(h).to_string()) // 去掉 :port
        .unwrap_or_default(); // 缺失则空串
    let path_and_query = req // 取原始 path?query
        .uri() // 请求 URI
        .path_and_query() // 路径与查询
        .map(|p| p.as_str()) // 转字符串
        .unwrap_or("/"); // 兜底 "/"
    let target = if https_port == 443 { // 标准端口省略端口号
        format!("https://{host}{path_and_query}") // 无端口形式
    } else {
        format!("https://{host}:{https_port}{path_and_query}") // 带端口形式
    };
    (StatusCode::PERMANENT_REDIRECT, [(LOCATION, target)]).into_response() // 返回 308 + Location
}
