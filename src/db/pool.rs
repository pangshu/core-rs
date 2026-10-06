//! 连接池：把 `DatabaseSettings` 变成 `DatabaseConnection`（池参数 + 慢查询日志）。
//! 只做这一件事（文档 三·5）；url 为空时由调用方（App 装配器）跳过。

use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseConnection, DbErr};

use crate::config::sections::DatabaseSettings;

pub async fn connect(cfg: &DatabaseSettings) -> Result<DatabaseConnection, DbErr> {
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
