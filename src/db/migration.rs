//! 迁移入口。
//!
//! aurora 提供迁移的**执行与状态查询**；迁移脚本由应用侧声明（见 [`migrations`]）。
//!
//! - 开发期：`sea-orm-cli migrate up`，或调用 [`up`] / [`down`] / [`status`]。
//! - 启动期：`[database].migrate_on_start = true` → [`migrate_on_start`]
//!   （启动状态机第 4 步，**绑定监听之前**）。
//!
//! [`migrations`]: crate::db::migrations

pub use sea_orm_migration::MigratorTrait;

use crate::boot::startup::StartupError;
use crate::db::pool::DbPool;
use crate::errors::AppResult;

/// 执行全部未应用的迁移（`up`）。
///
/// # Errors
/// 迁移失败返回映射后的 [`crate::errors::AppError`]（`50002`）。
pub async fn up<M: MigratorTrait>(db: &DbPool) -> AppResult<()> {
    M::up(db, None).await?;
    Ok(())
}

/// 回滚最近 `steps` 个迁移。
///
/// # Errors
/// 迁移失败返回映射后的 [`crate::errors::AppError`]（`50002`）。
pub async fn down<M: MigratorTrait>(db: &DbPool, steps: u32) -> AppResult<()> {
    M::down(db, Some(steps)).await?;
    Ok(())
}

/// 打印迁移状态（已应用 / 待应用）。
///
/// # Errors
/// 查询失败返回映射后的 [`crate::errors::AppError`]（`50002`）。
pub async fn status<M: MigratorTrait>(db: &DbPool) -> AppResult<()> {
    M::status(db).await?;
    Ok(())
}

/// 按配置决定是否在启动时执行迁移（启动状态机第 4 步）。
///
/// # Errors
/// 迁移失败返回 [`StartupError::Migration`]。
pub async fn migrate_on_start<M: MigratorTrait>(
    db: &DbPool,
    enabled: bool,
) -> Result<(), StartupError> {
    if !enabled {
        return Ok(());
    }
    tracing::info!("running database migrations");
    up::<M>(db)
        .await
        .map_err(|e| StartupError::Migration(e.to_string()))
}
