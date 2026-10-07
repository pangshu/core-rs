//! 固定窗口限流（feature = "rate-limit"）：按客户端 IP 经 cache `INCRBY` 计数，
//! 首次计数设定窗口 TTL；超限返回 429 + `Retry-After`。
//!
//! 阈值经 [`HasConfig`] 每请求读取——**支持热更新**（文档 三·4）。
//! 多实例部署需 redis 后端（memory 后端计数进程内有效）。
//!
//! 依赖锚点：经请求 extension 读取 `CoreState`（`App::serve` 挂在最外层；
//! 裸模式自组装时同样由框架必需件保证存在，缺扩展时 500 fail-closed）。

use std::time::Duration; // 引入时长类型，用于设置窗口 TTL

use axum::extract::{Extension, Request}; // 引入扩展提取器与请求类型
use axum::http::StatusCode; // 引入 HTTP 状态码类型
use axum::middleware::Next; // 引入中间件链的下一个处理器
use axum::response::{IntoResponse, Response}; // 引入可转响应 trait 与响应类型

use crate::state::CoreState; // 引入框架核心状态（依赖锚点）
use crate::traits::{HasCache, HasConfig}; // 引入缓存与配置能力 trait

pub(crate) async fn handle(Extension(core): Extension<CoreState>, req: Request, next: Next) -> Response { // 固定窗口限流中间件主体
    let settings = core.config().load().server.rate_limit.clone(); // 读取限流配置快照（支持热更新）
    if !settings.enabled { // 限流未开启时
        return next.run(req).await; // 直接放行
    }

    let peer = req // 取连接对端 IP 作为候选
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let mode = crate::utils::client_ip::IpKeyMode::parse(&core.config().load().server.ip_key_mode) // 解析 IP 取值模式
        .unwrap_or(crate::utils::client_ip::IpKeyMode::PeerIp); // 解析失败默认用对端 IP
    let Some(ip) = (match mode { // 按模式确定限流所用的客户端 IP
        crate::utils::client_ip::IpKeyMode::PeerIp => peer, // 对端模式直接用连接 IP
        crate::utils::client_ip::IpKeyMode::ProxyHeaders => { // 反代头模式
            crate::utils::client_ip::resolve(req.headers(), peer) // 优先按反代头解析，回退对端 IP
        }
    }) else { // 无法确定 IP 时
        return next.run(req).await; // 放弃限流直接放行
    };

    let bucket = if settings.bucket.is_empty() { // 未配置桶名时
        "default".to_string() // 使用默认桶
    } else {
        settings.bucket.clone() // 否则用配置的桶名
    };
    let window = settings.window_secs.max(1); // 窗口秒数（至少 1 秒）
    // 固定窗口 key：按当前窗口起点取整，TTL 到窗口结束
    let now = crate::utils::time::now_secs(); // 取当前秒级时间戳
    let window_start = now - now % window as i64; // 对齐到当前窗口起点
    let key = format!("core-rs:rl:{bucket}:{ip}:{window_start}"); // 拼出该窗口内的计数 key

    let cache = core.cache(); // 取缓存句柄
    match cache.incr(&key, 1).await { // 原子自增计数
        Ok(count) => { // 计数成功
            if count == 1 { // 窗口首次计数
                let _ = cache // 忽略设置 TTL 的结果
                    .expire(&key, Some(Duration::from_secs(window + 1))) // 设 TTL 到窗口结束后 1 秒
                    .await;
            }
            if count > settings.limit as i64 { // 超过阈值
                let retry_after = (window as i64 - (now - window_start)).max(1); // 计算建议重试秒数
                return ( // 返回 429 响应
                    StatusCode::TOO_MANY_REQUESTS, // 状态码 429
                    [("retry-after", retry_after.to_string())], // 带 Retry-After 头
                    axum::Json(crate::web::response::ApiResponse::error( // 统一错误响应体
                        429, // 业务码 429
                        "too many requests", // 固定提示消息
                    )),
                )
                    .into_response(); // 元组组合为 HTTP 响应并提前返回
            }
        }
        // 缓存故障降级放行（与 get_or_load 同一哲学：缓存抖动不传染成业务 500）
        Err(e) => { // 计数失败
            tracing::warn!(error = %e, "rate limit counter unavailable, allowing request"); // 告警后放行
        }
    }

    next.run(req).await // 未超限则继续下游处理
}

/// 自组装用层（裸模式）：固定窗口限流。生效与否由 `[server.rate_limit] enabled`
/// 配置决定（支持热更新）。
pub fn layer() -> super::BoxedLayer { // 返回装箱的限流层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}
