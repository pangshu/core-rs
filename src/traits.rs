//! 解耦点（文档 一）：框架的中间件与提取器对状态只要求实现这里的 trait，
//! 不认识应用类型本身——应用随意扩充状态而框架零改动。
//!
//! 应用侧写法（AppState 内嵌 CoreState 后，一行一个）：
//!
//! ```rust,ignore
//! impl HasDb for AppState { fn db(&self) -> Option<&DatabaseConnection> { self.core.db() } }
//! impl HasCache for AppState { fn cache(&self) -> &CacheHandle { self.core.cache() } }
//! ```

use std::sync::Arc; // 引入标准库的原子引用计数指针 Arc，用于跨线程共享所有权

use sea_orm::DatabaseConnection; // 引入 SeaORM 的数据库连接类型，trait 方法签名需要用到

use crate::cache::{CacheHandle, LockHandle}; // 引入框架缓存句柄与分布式锁句柄类型
use crate::config::{ConfigHandle, Settings}; // 引入配置句柄与全局设置类型
use crate::queue::QueueHandle; // 引入队列句柄类型

/// 有数据库：未配置数据源时返回 None（相关提取器报明确的错误）
pub trait HasDb { // 定义「能提供数据库连接」的能力 trait，应用状态实现它即可被框架使用
    fn db(&self) -> Option<&DatabaseConnection>; // 返回数据库连接的可选引用，未配置数据源时为 None
}

/// 有缓存与锁（同一 cache 后端配对提供）
pub trait HasCache { // 定义「能提供缓存与锁」的能力 trait
    fn cache(&self) -> &CacheHandle; // 返回进程内/分布式缓存句柄
    fn lock(&self) -> &LockHandle; // 返回与缓存后端配对的分布式锁句柄
}

/// 有队列
pub trait HasQueue { // 定义「能提供消息队列」的能力 trait
    fn queue(&self) -> &QueueHandle; // 返回队列句柄，用于发布/订阅消息
}

/// 有配置句柄（读取零锁：`state.config().load().server.port`）
pub trait HasConfig { // 定义「能提供配置句柄」的能力 trait
    fn config(&self) -> &ConfigHandle<Settings>; // 返回可无锁读取的配置句柄
}

/// 有认证链（auth 中间件 / 登录态判断使用）
pub trait HasAuth { // 定义「能提供认证器」的能力 trait
    fn authn(&self) -> Option<Arc<dyn crate::auth::Authn>>; // 返回认证器（session/jwt/oauth2 组合），未启用时为 None
}

/// 授权句柄：casbin feature 开启时为真实强制器；关闭时为占位单元——
/// 统一 S 的 trait 上界，应用的 trait 实现不随 feature 摆动
#[cfg(feature = "casbin")] // 仅在开启 casbin feature 时编译下面这行
pub type AuthzHandle = Arc<crate::authz::Enforcer>; // 开启 casbin 时授权句柄是 Casbin 强制器的 Arc 包装
#[cfg(not(feature = "casbin"))] // 在未开启 casbin feature 时编译下面这行
pub type AuthzHandle = (); // 关闭 casbin 时用单元类型占位，保证类型签名不随 feature 变化

/// 有授权强制器（middleware/authz 与 handler 权限检查使用）
pub trait HasAuthz { // 定义「能提供授权强制器」的能力 trait
    fn authz(&self) -> Option<&AuthzHandle>; // 返回授权句柄引用，未启用授权时为 None
}

/// 有实时通信 hub（feature = "ws" / "sse"）
#[cfg(any(feature = "ws", feature = "sse"))] // 只要开启 ws 或 sse 任一 feature 才编译下面的 trait
pub trait HasRealtime { // 定义「能提供实时通信中心」的能力 trait
    fn hub(&self) -> &Arc<crate::realtime::hub::Hub>; // 返回实时通信 Hub 的共享引用
}

/// 有自定义健康探针（/ready 聚合；返回注册快照）
pub trait HasHealthChecks { // 定义「能提供健康探针列表」的能力 trait
    fn health_checks(&self) -> Vec<Arc<dyn crate::observability::health::HealthCheck>>; // 返回当前注册的所有健康探针快照
}

// CoreState 自身当然实现全部 trait（应用 AppState 内嵌它后，各方法直接转发）
impl HasDb for crate::state::CoreState { // 为框架核心状态 CoreState 实现 HasDb
    fn db(&self) -> Option<&DatabaseConnection> { // 实现 db 方法
        self.db.as_ref() // 把内部存储的 Option<DatabaseConnection> 转成可选引用返回
    }
}

impl HasCache for crate::state::CoreState { // 为 CoreState 实现 HasCache
    fn cache(&self) -> &CacheHandle { // 实现 cache 方法
        &self.cache // 直接借用内部的缓存句柄
    }
    fn lock(&self) -> &LockHandle { // 实现 lock 方法
        &self.lock // 直接借用内部的锁句柄
    }
}

impl HasQueue for crate::state::CoreState { // 为 CoreState 实现 HasQueue
    fn queue(&self) -> &QueueHandle { // 实现 queue 方法
        &self.queue // 直接借用内部的队列句柄
    }
}

impl HasConfig for crate::state::CoreState { // 为 CoreState 实现 HasConfig
    fn config(&self) -> &ConfigHandle<Settings> { // 实现 config 方法
        &self.config // 直接借用内部的配置句柄
    }
}

impl HasAuth for crate::state::CoreState { // 为 CoreState 实现 HasAuth
    fn authn(&self) -> Option<Arc<dyn crate::auth::Authn>> { // 实现 authn 方法
        self.auth.clone() // 克隆内部认证器的 Arc（引用计数 +1）后返回
    }
}

impl HasAuthz for crate::state::CoreState { // 为 CoreState 实现 HasAuthz
    #[cfg(feature = "casbin")] // 开启 casbin 时使用下面这版实现
    fn authz(&self) -> Option<&AuthzHandle> { // 实现 authz 方法（casbin 版）
        self.authz.as_ref() // 把内部授权器 Option 转为可选引用
    }
    #[cfg(not(feature = "casbin"))] // 未开启 casbin 时使用下面这版实现
    fn authz(&self) -> Option<&AuthzHandle> { // 实现 authz 方法（占位版）
        None // 未启用授权，恒返回 None
    }
}

#[cfg(any(feature = "ws", feature = "sse"))] // 开启 ws 或 sse 时才编译下面的 impl
impl HasRealtime for crate::state::CoreState { // 为 CoreState 实现 HasRealtime
    fn hub(&self) -> &Arc<crate::realtime::hub::Hub> { // 实现 hub 方法
        &self.hub // 直接借用内部的实时通信 Hub
    }
}

impl HasHealthChecks for crate::state::CoreState { // 为 CoreState 实现 HasHealthChecks
    fn health_checks(&self) -> Vec<Arc<dyn crate::observability::health::HealthCheck>> { // 实现 health_checks 方法
        self.health_checks // 访问内部探针列表（读写锁保护的 Vec）
            .read() // 获取读锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // 若锁被 poison（持锁线程 panic）则取出内部值，避免连锁 panic
            .clone() // 克隆一份探针列表作为快照返回，避免长时间持锁
    }
}
