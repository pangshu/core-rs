//! 迁移装配（feature = "migration"）：接入应用侧 `migrations/` 目录。
//!
//! - 启动自动执行：`App` 构建器 `.migrations(Migrator)`；
//! - 手动执行：项目里加一个 `src/bin/migrate.rs`（约 5 行）调用本模块函数，
//!   `cargo run --bin migrate` 即可（框架无法提供统一 CLI，因为 Migrator
//!   类型定义在使用方项目里）。

use sea_orm::DatabaseConnection; // 引入数据库连接类型
use sea_orm_migration::MigratorTrait; // 引入迁移器 trait，提供 up/down/fresh/status 等方法

use crate::error::{AppError, AppResult}; // 引入框架错误类型与结果别名

/// 执行所有未应用的迁移
pub async fn up<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> { // 泛型迁移器 M，执行全部待应用迁移
    M::up(db, None) // 调用迁移器的 up，None 表示执行全部步骤
        .await // 等待迁移执行完成
        .map_err(|e| AppError::internal(format!("migration up failed: {e}")))?; // 失败时包装为内部错误
    tracing::info!(migrator = std::any::type_name::<M>(), "migrations applied"); // 记录已应用迁移的日志（含迁移器类型名）
    Ok(()) // 返回成功
}

/// 回滚最近一个已应用的迁移
pub async fn down<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> { // 泛型迁移器 M，回滚最近一步
    M::down(db, None) // 调用迁移器的 down，None 表示回滚一步
        .await // 等待回滚完成
        .map_err(|e| AppError::internal(format!("migration down failed: {e}")))?; // 失败时包装为内部错误
    Ok(()) // 返回成功
}

/// 重建数据库：全部回滚后重新执行所有迁移
pub async fn fresh<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> { // 泛型迁移器 M，重建数据库
    M::fresh(db) // 调用迁移器的 fresh（全部回滚再重跑）
        .await // 等待完成
        .map_err(|e| AppError::internal(format!("migration fresh failed: {e}")))?; // 失败时包装为内部错误
    Ok(()) // 返回成功
}

/// 查看迁移状态（已应用 / 待应用）
pub async fn status<M: MigratorTrait>(db: &DatabaseConnection) -> AppResult<()> { // 泛型迁移器 M，打印迁移状态
    M::status(db) // 调用迁移器的 status
        .await // 等待完成
        .map_err(|e| AppError::internal(format!("migration status failed: {e}")))?; // 失败时包装为内部错误
    Ok(()) // 返回成功
}
