//! 数据库连接池初始化与 `Db` 提取器。
//!
//! handler 里直接声明 `db: Db` 拿连接；`&*db` / `&db.0` 传给 sea-orm 查询。

use std::time::Duration;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use sea_orm::{ConnectOptions, Database, DatabaseConnection, DbErr};

use crate::config::DatasourceConfig;
use crate::error::AppError;
use crate::state::AppState;

/// 按配置建立连接池。url 为空时由调用方（app 构建器）跳过，不会走到这里。
pub async fn connect(cfg: &DatasourceConfig) -> Result<DatabaseConnection, DbErr> {
    let mut opts = ConnectOptions::new(cfg.url.clone());
    if cfg.max_connections > 0 {
        opts.max_connections(cfg.max_connections);
    }
    if cfg.min_connections > 0 {
        opts.min_connections(cfg.min_connections);
    }
    // 池耗尽时的获取超时：默认 0 = 用底层默认值（30s）
    if cfg.connect_timeout_secs > 0 {
        opts.connect_timeout(Duration::from_secs(cfg.connect_timeout_secs));
    }
    if cfg.idle_timeout_secs > 0 {
        opts.idle_timeout(Some(Duration::from_secs(cfg.idle_timeout_secs)));
    }
    if cfg.max_lifetime_secs > 0 {
        opts.max_lifetime(Some(Duration::from_secs(cfg.max_lifetime_secs)));
    }

    // SQL 日志与慢查询记录（经 tracing 输出，受 [log].level 过滤）
    // sea-orm 只在 sqlx_logging 开启时才装配慢查询日志：sql_logging 关闭但
    // slow_query_ms 开启时，把常规语句级别设为 Off，使慢 SQL 仍可单独输出
    if cfg.sql_logging {
        opts.sqlx_logging(true);
        opts.sqlx_logging_level(log::LevelFilter::Debug);
    } else if cfg.slow_query_ms > 0 {
        opts.sqlx_logging(true);
        opts.sqlx_logging_level(log::LevelFilter::Off);
    } else {
        opts.sqlx_logging(false);
    }
    if cfg.slow_query_ms > 0 {
        opts.sqlx_slow_statements_logging_settings(
            log::LevelFilter::Warn,
            Duration::from_millis(cfg.slow_query_ms),
        );
    }

    Database::connect(opts).await
}

/// 数据库连接提取器。未配置数据源时返回内部错误（500）。
#[derive(Debug, Clone)]
pub struct Db(pub DatabaseConnection);

impl Db {
    pub fn inner(&self) -> &DatabaseConnection {
        &self.0
    }
}

impl std::ops::Deref for Db {
    type Target = DatabaseConnection;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromRequestParts<S> for Db
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(_parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        AppState::from_ref(state)
            .db
            .clone()
            .map(Db)
            .ok_or_else(|| AppError::internal("datasource not configured"))
    }
}
