//! 测试辅助（feature = "test-util"）：一行拿到测试态 [`AppState`]，避免每个业务项目
//! 手搓连接与状态。业务项目在 `dev-dependencies` 中启用：
//!
//! ```toml
//! [dev-dependencies.core-rs]
//! features = ["test-util"]
//! ```
//!
//! ```no_run
//! use core_rs::test;
//! use core_rs::prelude::*;
//!
//! #[tokio::test]
//! async fn my_handler_test() {
//!     let state = test::memory_db_state().await;   // sqlite 内存库 + 内存缓存
//!     let app = Router::new().merge(user::routes()).with_state(state);
//!     // tower::ServiceExt::oneshot 发请求断言……
//! }
//! ```

use std::sync::Arc;

use sea_orm::DatabaseConnection;

use crate::config::AppConfig;
use crate::state::AppState;

/// 全默认零配置（无 db / redis / jwt；cache 按 feature 与 cache.type 默认值装配）
pub fn config() -> AppConfig {
    AppConfig::default()
}

/// sqlite 内存库连接（单连接池：内存库多连接互不相通）
pub async fn memory_db() -> DatabaseConnection {
    crate::orm::pool::connect(&crate::config::DatasourceConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .expect("in-memory sqlite connect failed")
}

/// 按配置装配测试态 AppState（与 [`crate::Application`] 相同的 db / 缓存 / jwt 判定逻辑，
/// 但不启动服务）。需要表的用例先自行跑迁移：`Migrator::up(&*state.db.unwrap()).await`。
pub async fn state(config: AppConfig) -> AppState {
    let db = if config.datasource.url.is_empty() {
        None
    } else {
        Some(
            crate::orm::pool::connect(&config.datasource)
                .await
                .expect("test db connect failed"),
        )
    };
    let cache = crate::cache::build(&config.cache, &config.redis).expect("test cache build failed");

    #[cfg(feature = "jwt")]
    let jwt = if config.jwt.secret.is_empty() {
        None
    } else {
        Some(crate::security::Jwt::new(&config.jwt).expect("test jwt init failed"))
    };

    AppState {
        config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
        db,
        cache,
        #[cfg(feature = "queue")]
        queue: None,
        #[cfg(feature = "jwt")]
        jwt,
    }
}

/// 常用快捷态：默认配置 + sqlite 内存库（cache-memory feature 开启时附带内存缓存）
pub async fn memory_db_state() -> AppState {
    let mut config = config();
    config.datasource.url = "sqlite::memory:".to_string();
    config.datasource.max_connections = 1;
    state(config).await
}
