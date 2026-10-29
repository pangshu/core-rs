//! 数据库连接池（SeaORM `DatabaseConnection`）。

use async_trait::async_trait;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

use crate::boot::startup::StartupError;
use crate::config::sections::database::DatabaseSection;
use crate::errors::AppResult;
use crate::telemetry::HealthCheck;

/// SeaORM 数据库连接池。
///
/// 以 `Arc` 共享，`Clone` 廉价；可直接用于 handler 的 `State<DbPool>`。
pub type DbPool = DatabaseConnection;

/// 按配置连接数据库（启动状态机第 3 步）。
///
/// # Errors
/// 建连失败返回 [`StartupError::Resource`]。
pub async fn connect(cfg: &DatabaseSection) -> Result<DbPool, StartupError> {
    let mut options = ConnectOptions::new(cfg.url.clone());
    options
        .max_connections(cfg.max_connections)
        .min_connections(cfg.min_connections)
        .connect_timeout(cfg.connect_timeout)
        .acquire_timeout(cfg.acquire_timeout)
        .idle_timeout(cfg.idle_timeout)
        .max_lifetime(cfg.max_lifetime)
        .sqlx_logging(false);

    let db = Database::connect(options)
        .await
        // 驱动原文可能带完整连接串（含口令）：进日志前先抹掉 userinfo
        .map_err(|e| {
            StartupError::Resource(format!(
                "database connect failed: {}",
                crate::utils::string::redact_url_secrets(&e.to_string())
            ))
        })?;

    tracing::info!("database connected");
    Ok(db)
}

/// ping 数据库（连接可用性）。
///
/// # Errors
/// 连接不可用时返回映射后的 [`crate::errors::AppError`]（`50002`）。
pub async fn ping(db: &DbPool) -> AppResult<()> {
    db.ping().await?;
    Ok(())
}

/// 数据库就绪探针（供 `/ready` 使用）。
///
/// `Err` 里给的是 `DbErr` 原文（可能含用户名、库名、表名与驱动信息）：注册表
/// （`telemetry::HealthRegistry::run`）会把它**只写进日志**，对外统一替换成
/// `database unavailable`——`/ready` 对未认证调用方可见。
#[derive(Debug, Clone)]
pub struct DbHealth {
    db: DbPool,
}

impl DbHealth {
    /// 由连接池构造探针。
    #[must_use]
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl HealthCheck for DbHealth {
    fn name(&self) -> &str {
        "database"
    }

    async fn check(&self) -> Result<(), String> {
        self.db.ping().await.map_err(|e| e.to_string())
    }
}
