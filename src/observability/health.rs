//! health（文档 三·8）：`/health`（liveness，进程在即 ok）与 `/ready`
//! （readiness，依次探 DB 与已启用的缓存/队列后端，任一失败 503）。
//! 探针项通过 [`HealthCheck`] trait 注册（`CoreState::register_health_check`），
//! 应用可追加自定义探针（如对象存储）。
//!
//! health/metrics 路由由框架在 `App::serve()` 时自动挂载，应用无需关心。

use std::sync::Arc; // 引入 Arc，用于共享自定义探针

use axum::extract::State; // 引入状态提取器，从应用状态取探针
use axum::http::StatusCode; // 引入 HTTP 状态码
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型
use axum::{Json, Router}; // 引入 JSON 响应与路由器
use serde_json::json; // 引入 json! 宏构造响应体

use crate::traits::{HasCache, HasDb, HasHealthChecks, HasQueue}; // 引入健康检查所需的状态能力 trait

/// 自定义探针契约：name 用于 /ready 的 custom 字段，check 返回存活状态
#[async_trait::async_trait] // 让 trait 支持 async 方法
pub trait HealthCheck: Send + Sync { // 自定义健康探针契约
    fn name(&self) -> &str; // 探针名称（作为 custom 字段键）
    async fn check(&self) -> HealthStatus; // 执行探测并返回状态
}

/// 探针结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)] // 派生调试/克隆/拷贝/相等
pub enum HealthStatus { // 健康状态枚举
    Up, // 正常
    Down, // 异常
}

/// /health（liveness）：进程在即返回 ok
pub async fn liveness() -> Response { // 存活探针处理函数
    (
        StatusCode::OK, // 恒返回 200
        Json(crate::web::response::ApiResponse::ok("ok")), // 统一响应体
    )
        .into_response() // 转换为 axum 响应
}

/// /ready（readiness）：逐一探测 db / cache / queue 与注册的自定义探针。
/// 任一**已配置**组件 down 时返回 503，便于 k8s / LB 就绪探针直接摘除实例；
/// 未配置的组件不参与判定。
pub(crate) async fn readiness<S>(State(state): State<S>) -> Response // 就绪探针处理函数（泛型状态）
where // 泛型约束
    S: HasDb + HasCache + HasQueue + HasHealthChecks + Clone + Send + Sync + 'static, // 状态需提供各组件能力
{
    let db = match state.db() { // 探测数据库
        None => "not_configured".to_string(), // 未配置数据库则不参与判定
        Some(db) => match db.ping().await { // 已配置则 ping
            Ok(()) => "up".to_string(), // ping 成功
            Err(e) => { // ping 失败
                tracing::warn!(error = %e, "db ping failed"); // 记录告警
                "down".to_string() // 标记为 down
            }
        },
    };

    let cache = match state.cache().ping().await { // 探测缓存后端
        Ok(()) => "up".to_string(), // ping 成功
        Err(e) => { // ping 失败
            tracing::warn!(error = %e, "cache ping failed"); // 记录告警
            "down".to_string() // 标记为 down
        }
    };

    let queue = match state.queue().ping().await { // 探测队列后端
        Ok(()) => "up".to_string(), // ping 成功
        Err(e) => { // ping 失败
            tracing::warn!(error = %e, "queue ping failed"); // 记录告警
            "down".to_string() // 标记为 down
        }
    };

    let mut custom = Vec::new(); // 收集自定义探针结果
    for check in state.health_checks() { // 遍历注册的自定义探针
        let status = match check.check().await { // 执行单个探针
            HealthStatus::Up => "up".to_string(), // 正常
            HealthStatus::Down => "down".to_string(), // 异常
        };
        custom.push((check.name().to_string(), status)); // 记录 (名称, 状态)
    }

    let degraded = db == "down" // 数据库 down
        || cache == "down" // 或缓存 down
        || queue == "down" // 或队列 down
        || custom.iter().any(|(_, s)| s == "down"); // 或任一定制探针 down
    let status = if degraded { // 依据整体状态选择 HTTP 码
        StatusCode::SERVICE_UNAVAILABLE // 有组件异常 → 503
    } else {
        StatusCode::OK // 全部正常 → 200
    };
    let body = crate::web::response::ApiResponse { // 组装统一响应体
        code: if degraded { 503 } else { crate::web::response::CODE_OK }, // 业务码与 HTTP 码一致
        message: if degraded { // 提示信息
            "component down".to_string() // 有组件异常
        } else {
            "ok".to_string() // 全部正常
        },
        data: json!({ // 各组件探测明细
            "db": db, // 数据库状态
            "cache": cache, // 缓存状态
            "queue": queue, // 队列状态
            "custom": custom, // 自定义探针状态
        }),
    };
    (status, Json(body)).into_response() // 组合状态码与响应体
}

/// 框架健康路由（泛型状态）：/health + /ready
pub fn routes<S>() -> Router<S> // 构造健康检查路由
where // 泛型约束
    S: HasDb + HasCache + HasQueue + HasHealthChecks + Clone + Send + Sync + 'static, // 状态需提供各组件能力
{
    Router::new() // 新建路由器
        .route("/health", axum::routing::get(liveness)) // 挂载存活探针
        .route("/ready", axum::routing::get(readiness::<S>)) // 挂载就绪探针
}

// Arc 供 HealthCheck 注册与读取使用
#[allow(unused)] // 类型别名可能暂未被引用
type HealthCheckHandle = Arc<dyn HealthCheck>; // 探针的共享句柄别名
