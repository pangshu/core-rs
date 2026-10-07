//! 连接池：把 `DatabaseSettings` 变成 `DatabaseConnection`（池参数 + 慢查询日志）。
//! 只做这一件事（文档 三·5）；url 为空时由调用方（App 装配器）跳过。

use std::time::Duration; // 引入时长类型，用于各连接池超时配置

use sea_orm::{ConnectOptions, Database, DatabaseConnection, DbErr}; // 引入 SeaORM 连接选项、数据库入口、连接与错误类型

use crate::config::sections::DatabaseSettings; // 引入配置中的数据库设置结构体

pub async fn connect(cfg: &DatabaseSettings) -> Result<DatabaseConnection, DbErr> { // 依据配置异步建立数据库连接池，失败返回 DbErr
    let mut opts = ConnectOptions::new(cfg.url.clone()); // 用配置中的 URL 创建可变的连接选项
    if cfg.max_connections > 0 { // 仅当配置了正数上限时才覆盖
        opts.max_connections(cfg.max_connections); // 设置连接池最大连接数
    }
    if cfg.min_connections > 0 { // 仅当配置了正数下限时才覆盖
        opts.min_connections(cfg.min_connections); // 设置连接池最小空闲连接数
    }
    // 池耗尽时的获取超时：默认 0 = 用底层默认值（30s）
    if cfg.connect_timeout_secs > 0 { // 配置了正的获取超时秒数时生效
        opts.connect_timeout(Duration::from_secs(cfg.connect_timeout_secs)); // 设置从池中获取连接的超时
    }
    if cfg.idle_timeout_secs > 0 { // 配置了正的空闲超时秒数时生效
        opts.idle_timeout(Some(Duration::from_secs(cfg.idle_timeout_secs))); // 设置空闲连接被回收的时长
    }
    if cfg.max_lifetime_secs > 0 { // 配置了正的最大存活秒数时生效
        opts.max_lifetime(Some(Duration::from_secs(cfg.max_lifetime_secs))); // 设置单个连接的最大存活时长
    }

    // SQL 日志与慢查询记录（经 tracing 输出，受 [log].level 过滤）
    // sea-orm 只在 sqlx_logging 开启时才装配慢查询日志：sql_logging 关闭但
    // slow_query_ms 开启时，把常规语句级别设为 Off，使慢 SQL 仍可单独输出
    if cfg.sql_logging { // 开启 SQL 日志时输出常规语句日志
        opts.sqlx_logging(true); // 打开 sqlx 语句日志
        opts.sqlx_logging_level(log::LevelFilter::Debug); // 常规 SQL 以 Debug 级别输出
    } else if cfg.slow_query_ms > 0 { // 未开常规日志但配了慢查询阈值时
        opts.sqlx_logging(true); // 仍需打开 sqlx 日志以便捕获慢查询
        opts.sqlx_logging_level(log::LevelFilter::Off); // 常规语句日志关闭，仅保留慢查询输出
    } else { // 两者都没配置时
        opts.sqlx_logging(false); // 完全关闭 sqlx 日志
    }
    if cfg.slow_query_ms > 0 { // 配置了慢查询阈值时装配慢查询记录
        opts.sqlx_slow_statements_logging_settings( // 设置慢查询日志级别与阈值
            log::LevelFilter::Warn, // 慢查询以 Warn 级别输出
            Duration::from_millis(cfg.slow_query_ms), // 超过该毫秒数视为慢查询
        );
    }

    Database::connect(opts).await // 按最终选项建立连接并返回（异步等待）
}
