//! 解耦点（文档 一）：框架的中间件与提取器对状态只要求实现这里的 trait，
//! 不认识应用类型本身——应用随意扩充状态而框架零改动。
//!
//! 应用侧写法（AppState 内嵌 CoreState 后，一行一个）：
//!
//! ```rust,ignore
//! impl HasDb for AppState { fn db(&self) -> Option<&DatabaseConnection> { self.core.db() } }
//! impl HasCache for AppState { fn cache(&self) -> &CacheHandle { self.core.cache() } }
//! ```

use std::sync::Arc;

use sea_orm::DatabaseConnection;

use crate::cache::{CacheHandle, LockHandle};
use crate::config::{ConfigHandle, Settings};
use crate::queue::QueueHandle;

/// 有数据库：未配置数据源时返回 None（相关提取器报明确的错误）
pub trait HasDb {
    fn db(&self) -> Option<&DatabaseConnection>;
}

/// 有缓存与锁（同一 cache 后端配对提供）
pub trait HasCache {
    fn cache(&self) -> &CacheHandle;
    fn lock(&self) -> &LockHandle;
}

/// 有队列
pub trait HasQueue {
    fn queue(&self) -> &QueueHandle;
}

/// 有配置句柄（读取零锁：`state.config().load().server.port`）
pub trait HasConfig {
    fn config(&self) -> &ConfigHandle<Settings>;
}

/// 有认证链（auth 中间件 / 登录态判断使用）
pub trait HasAuth {
    fn authn(&self) -> Option<Arc<dyn crate::auth::Authn>>;
}

/// 授权句柄：casbin feature 开启时为真实强制器；关闭时为占位单元——
/// 统一 S 的 trait 上界，应用的 trait 实现不随 feature 摆动
#[cfg(feature = "casbin")]
pub type AuthzHandle = Arc<crate::authz::Enforcer>;
#[cfg(not(feature = "casbin"))]
pub type AuthzHandle = ();

/// 有授权强制器（middleware/authz 与 handler 权限检查使用）
pub trait HasAuthz {
    fn authz(&self) -> Option<&AuthzHandle>;
}

/// 有实时通信 hub（feature = "ws" / "sse"）
#[cfg(any(feature = "ws", feature = "sse"))]
pub trait HasRealtime {
    fn hub(&self) -> &Arc<crate::realtime::hub::Hub>;
}

/// 有自定义健康探针（/ready 聚合；返回注册快照）
pub trait HasHealthChecks {
    fn health_checks(&self) -> Vec<Arc<dyn crate::observability::health::HealthCheck>>;
}

// CoreState 自身当然实现全部 trait（应用 AppState 内嵌它后，各方法直接转发）
impl HasDb for crate::state::CoreState {
    fn db(&self) -> Option<&DatabaseConnection> {
        self.db.as_ref()
    }
}

impl HasCache for crate::state::CoreState {
    fn cache(&self) -> &CacheHandle {
        &self.cache
    }
    fn lock(&self) -> &LockHandle {
        &self.lock
    }
}

impl HasQueue for crate::state::CoreState {
    fn queue(&self) -> &QueueHandle {
        &self.queue
    }
}

impl HasConfig for crate::state::CoreState {
    fn config(&self) -> &ConfigHandle<Settings> {
        &self.config
    }
}

impl HasAuth for crate::state::CoreState {
    fn authn(&self) -> Option<Arc<dyn crate::auth::Authn>> {
        self.auth.clone()
    }
}

impl HasAuthz for crate::state::CoreState {
    #[cfg(feature = "casbin")]
    fn authz(&self) -> Option<&AuthzHandle> {
        self.authz.as_ref()
    }
    #[cfg(not(feature = "casbin"))]
    fn authz(&self) -> Option<&AuthzHandle> {
        None
    }
}

#[cfg(any(feature = "ws", feature = "sse"))]
impl HasRealtime for crate::state::CoreState {
    fn hub(&self) -> &Arc<crate::realtime::hub::Hub> {
        &self.hub
    }
}

impl HasHealthChecks for crate::state::CoreState {
    fn health_checks(&self) -> Vec<Arc<dyn crate::observability::health::HealthCheck>> {
        self.health_checks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
