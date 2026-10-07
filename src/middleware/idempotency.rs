//! 写接口幂等（文档 三·10）：读 `Idempotency-Key` 头，首次请求占位（锁）→
//! 执行 → 缓存响应；重复请求直接回放结果，避免写接口重放。
//!
//! 只作用于带 body 的写方法（POST/PUT/PATCH）且带 key 的请求；无 Redis 时
//! 自动降级为进程内实现（单机可用，多实例需 Redis）。
//!
//! 依赖锚点：经请求 extension 读取 `CoreState`（`App::serve` 挂在最外层；
//! 裸模式自组装时同样由框架必需件保证存在，缺扩展时 500 fail-closed）。
//! 本层须挂在 auth **内侧**：回放缓存按 Identity 隔离，防跨用户回放。

use std::time::Duration; // 引入时长类型，用于锁与缓存的 TTL

use axum::extract::{Extension, Request}; // 引入扩展提取器与请求体类型
use axum::http::{header, StatusCode}; // 引入常用响应头常量与状态码
use axum::middleware::Next; // 引入 Next，用于把请求交给下游中间件
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型

use crate::cache::CacheExt; // 引入缓存扩展方法（get_json/set_json）
use crate::state::CoreState; // 引入框架核心状态（依赖锚点）
use crate::traits::{HasCache, HasConfig}; // 引入状态能力 trait：取缓存与配置

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key"; // 幂等键请求头的常量名
/// 客户端提供的 key 长度上限：防恶意超长 key 撑爆缓存键空间
const MAX_KEY_LEN: usize = 255; // 幂等键最大字节数

pub(crate) async fn handle(Extension(core): Extension<CoreState>, req: Request, next: Next) -> Response { // 幂等中间件入口：取 Idempotency-Key，命中则回放，否则执行并缓存
    let idem = core.config().load().server.idempotency.clone(); // 加载并克隆幂等配置快照，避免长时间持有配置
    if !idem.enabled { // 幂等未启用
        return next.run(req).await; // 直接放行到下游
    }
    let is_write = matches!( // 判断是否为写方法（仅写方法参与幂等）
        *req.method(), // 解引用取请求方法
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH // 命中 POST/PUT/PATCH 之一
    );
    let key = req // 提取客户端提供的 Idempotency-Key 头
        .headers() // 访问请求头
        .get(IDEMPOTENCY_KEY_HEADER) // 按常量名取该头
        .and_then(|v| v.to_str().ok()) // 头值须为合法 UTF-8，否则视为无 key
        .unwrap_or("") // 缺失时用空串占位
        .trim() // 去除首尾空白
        .to_string(); // 转为自有 String 便于拼接键
    if !is_write || key.is_empty() { // 非写方法或无 key：不参与幂等
        return next.run(req).await; // 直接放行到下游
    }
    if key.len() > MAX_KEY_LEN { // key 超长：拒绝，防撑爆缓存键空间
        return ( // 构造 400 响应返回
            StatusCode::BAD_REQUEST, // 状态码 400
            axum::Json(crate::web::response::ApiResponse::error( // 使用统一 JSON 错误体
                400, // 业务错误码 400
                "Idempotency-Key too long (max 255 bytes)", // 错误信息（字符串字面量之外加注释）
            )),
        )
            .into_response(); // 转为 axum Response
    }

    // 缓存键按 身份+方法+路由 隔离：key 只来自客户端可伪造的请求头，
    // 不隔离则同 key 的不同用户/不同接口会互相回放对方的响应体
    let user = req // 取请求身份用于缓存键隔离（防跨用户回放）
        .extensions() // 访问请求扩展
        .get::<crate::auth::Identity>() // 取 auth 中间件注入的 Identity
        .map(|i| i.id.as_str()) // 映射为用户 id 字符串
        .unwrap_or("anon"); // 匿名请求用 "anon" 占位
    let scope = format!("{}:{}:{}", user, req.method(), req.uri().path()); // 隔离域：身份+方法+路由
    let cache_key = format!("core-rs:idem:{scope}:{key}"); // 回放结果的缓存键
    let lock_key = format!("core-rs:idem-lock:{scope}:{key}"); // 占位锁的键
    let ttl = Duration::from_secs(idem.ttl_secs.max(1)); // 回放与锁的 TTL，至少 1 秒

    // 已有回放结果：直接回放（锁外快路径）
    if let Ok(Some(replay)) = core.cache().get_json::<Replay>(&cache_key).await { // 锁外快路径：命中已有回放结果
        return replay.into_response(); // 回放上次响应
    }

    // 占位锁：并发同 key 请求中，后来者读到回放缓存或得到 409
    let guard = core.lock().clone().try_acquire(&lock_key, ttl).await; // 尝试获取占位锁（克隆句柄以便移动进异步调用）
    let guard = match guard { // 处理获取锁的三种结果
        Ok(Some(g)) => g, // 成功拿到锁
        Ok(None) => { // 锁被他人持有：并发同 key 请求
            return ( // 返回 409 冲突
                StatusCode::CONFLICT, // 状态码 409
                axum::Json(crate::web::response::ApiResponse::error( // 使用统一 JSON 错误体
                    409, // 业务错误码 409
                    "request with this Idempotency-Key is already in progress", // 错误信息
                )),
            )
                .into_response(); // 转为 Response
        }
        Err(e) => { // 锁故障（如后端不可用）
            tracing::warn!(error = %e, "idempotency lock unavailable, executing without guard"); // 记警告：锁不可用，降级直执
            // 锁故障降级直执（不阻塞业务）
            return execute_and_cache(core, req, next, cache_key, idem.max_body_bytes, ttl).await; // 无锁直接执行并缓存，保证业务可用
        }
    };

    // 拿到锁后二次读（双检）
    if let Ok(Some(replay)) = core.cache().get_json::<Replay>(&cache_key).await { // 拿锁后二次读缓存（双检，防期间已有结果写入）
        let _ = guard.release().await; // 命中则先释放锁
        return replay.into_response(); // 回放结果
    }
    let res = execute_and_cache(core, req, next, cache_key, idem.max_body_bytes, ttl).await; // 执行下游并缓存响应
    let _ = guard.release().await; // 释放占位锁
    res // 返回响应
}

/// 自组装用层（裸模式）：写接口幂等。须挂在 auth **内侧**（回放缓存按
/// Identity 隔离）；生效与否由 `[server.idempotency] enabled` 配置决定。
pub fn layer() -> super::BoxedLayer { // 返回装箱的幂等层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}

async fn execute_and_cache( // 执行下游并在成功时缓存可回放的响应
    core: CoreState, // 框架核心状态（取缓存句柄）
    req: Request, // 待转发请求
    next: Next, // 下游服务
    cache_key: String, // 回放缓存键
    max_body: usize, // 可缓存响应体上限
    ttl: Duration, // 缓存有效期
) -> Response // 返回下游响应
{
    let res = next.run(req).await; // 先执行下游拿到响应

    // 只缓存可回放的 JSON 响应（4xx/5xx 不缓存：失败允许重试）
    let status = res.status(); // 记录响应状态码
    if !status.is_success() { // 非 2xx 不缓存
        return res; // 原样返回失败响应
    }
    let (parts, body) = res.into_parts(); // 拆解响应为头部与 body
    // Content-Length 已知超限：不读 body，原样透传（不缓存，也绝不丢业务数据）
    if let Some(len) = parts // 读取 Content-Length 判断是否超限
        .headers // 访问响应头
        .get(header::CONTENT_LENGTH) // 取 Content-Length 头
        .and_then(|v| v.to_str().ok()) // 头值转字符串
        .and_then(|s| s.parse::<usize>().ok()) // 解析为 usize
    {
        if len > max_body { // 已知长度超过上限
            return Response::from_parts(parts, body); // 不读 body 原样透传，绝不丢业务数据
        }
    }
    match axum::body::to_bytes(body, max_body.max(1)).await { // 读取 body（上限 max_body）
        Ok(bytes) => { // 读取成功
            let replay = Replay { // 构造可回放记录
                status: status.as_u16(), // 记录状态码
                body: bytes.to_vec(), // 记录响应体字节
                content_type: parts // 记录 Content-Type
                    .headers // 访问响应头
                    .get(header::CONTENT_TYPE) // 取 Content-Type
                    .and_then(|v| v.to_str().ok()) // 头值转字符串
                    .unwrap_or("application/json") // 缺省按 JSON 处理
                    .to_string(), // 转自有 String
            };
            let _ = core.cache().set_json(&cache_key, &replay, Some(ttl)).await; // 写入缓存（失败忽略，不阻塞响应）
            Response::from_parts(parts, axum::body::Body::from(bytes)) // 用原头部与 body 重建响应返回
        }
        // 响应体超限（流式/无 Content-Length）：显式报错而非静默丢业务数据
        Err(_) => { // 读取失败：body 超限或流式
            tracing::warn!( // 记警告：无法回放
                limit = max_body, // 记录上限值
                "idempotency: response body exceeded max_body_bytes and cannot be replayed" // 警告信息
            );
            ( // 返回 500 错误响应
                StatusCode::INTERNAL_SERVER_ERROR, // 状态码 500
                axum::Json(crate::web::response::ApiResponse::error( // 使用统一 JSON 错误体
                    500, // 业务错误码 500
                    "response body too large to be made idempotent", // 错误信息
                )),
            )
                .into_response() // 转为 Response
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)] // 派生调试/克隆/序列化，便于缓存读写
struct Replay { // 可回放的响应记录
    status: u16, // 原响应状态码
    body: Vec<u8>, // 原响应体字节
    content_type: String, // 原响应 Content-Type
}

impl Replay { // 为 Replay 实现回放方法
    fn into_response(self) -> Response { // 把记录还原为 axum Response
        ( // 组装元组响应
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK), // 解析状态码，非法时退回 200
            [(axum::http::header::CONTENT_TYPE, self.content_type)], // 还原 Content-Type 头
            self.body, // 还原响应体
        )
            .into_response() // 转为 Response
    }
}
