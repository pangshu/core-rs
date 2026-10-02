//! 路由间共享的应用上下文。db / cache / queue / jwt 为 `Option`：对应能力未配置时，
//! 相关提取器在 handler 里拿到时返回明确的错误。
//!
//! `config` 是 [`arc_swap::ArcSwap`] 配置热切换单元（feature = "watch" 时随文件
//! 变更自动替换）：读取用 `state.config.load().server.port`，拿到的是当前生效
//! 快照，加载期间读方不受影响。

use std::sync::Arc;

use sea_orm::DatabaseConnection;

use crate::cache::Cache;
use crate::config::AppConfig;

#[cfg(feature = "queue")]
use crate::queue::QueueHandle;
#[cfg(feature = "jwt")]
use crate::security::Jwt;

#[derive(Clone)]
pub struct AppState {
    /// 热切换配置：`state.config.load()` 取当前生效快照（`Arc<AppConfig>`）
    pub config: Arc<arc_swap::ArcSwap<AppConfig>>,
    pub db: Option<DatabaseConnection>,
    pub cache: Option<Cache>,
    /// feature = "queue" 且 `[queue]` 后端可用时存在
    #[cfg(feature = "queue")]
    pub queue: Option<QueueHandle>,
    /// feature = "jwt" 且配置了 [jwt].secret 时存在
    #[cfg(feature = "jwt")]
    pub jwt: Option<Jwt>,
}

impl AppState {
    /// 未配置数据源时的占位状态（测试用）
    pub fn without_db(config: Arc<AppConfig>) -> Self {
        Self {
            config: Arc::new(arc_swap::ArcSwap::from_pointee((*config).clone())),
            db: None,
            cache: None,
            #[cfg(feature = "queue")]
            queue: None,
            #[cfg(feature = "jwt")]
            jwt: None,
        }
    }

    /// 以给定配置构造空状态（无 db / cache / queue / jwt）——测试与工具场景用；
    /// 完整装配走 [`crate::Application::builder`]
    pub fn new(config: AppConfig) -> Self {
        Self {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
            db: None,
            cache: None,
            #[cfg(feature = "queue")]
            queue: None,
            #[cfg(feature = "jwt")]
            jwt: None,
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "queue")]
        let queue_desc = self.queue.as_ref().map_or("none", |_| "configured");
        #[cfg(not(feature = "queue"))]
        let queue_desc = "unavailable";
        f.debug_struct("AppState")
            .field("db", &self.db.is_some())
            .field("cache", &self.cache.is_some())
            .field("queue", &queue_desc)
            .finish_non_exhaustive()
    }
}
