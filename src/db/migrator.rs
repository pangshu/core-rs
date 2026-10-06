//! 迁移装配（feature = "migration"）：接入应用侧 `migrations/` 目录。
//!
//! - 启动自动执行：`App` 构建器 `.migrations(Migrator)`；
//! - 手动执行：项目里加一个 `src/bin/migrate.rs`（约 5 行）调用本模块函数，
//!   `cargo run --bin migrate` 即可（框架无法提供统一 CLI，因为 Migrator
//!   类型定义在使用方项目里）。

use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;

use crate::error::{AppError, AppResult};

/// 执行所有未应用的迁移
pub async fn up<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> {
    M::up(db, None)
        .await
        .map_err(|e| AppError::internal(format!("migration up failed: {e}")))?;
    tracing::info!(migrator = std::any::type_name::<M>(), "migrations applied");
    Ok(())
}

/// 回滚最近一个已应用的迁移
pub async fn down<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> {
    M::down(db, None)
        .await
        .map_err(|e| AppError::internal(format!("migration down failed: {e}")))?;
    Ok(())
}

/// 重建数据库：全部回滚后重新执行所有迁移
pub async fn fresh<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> {
    M::fresh(db)
        .await
        .map_err(|e| AppError::internal(format!("migration fresh failed: {e}")))?;
    Ok(())
}

/// 查看迁移状态（已应用 / 待应用）
pub async fn status<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> {
    M::status(db)
        .await
        .map_err(|e| AppError::internal(format!("migration status failed: {e}")))?;
    Ok(())
}
