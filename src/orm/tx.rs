//! 事务便捷封装：闭包拿到 `&DatabaseTransaction`，成功提交、失败回滚。

use sea_orm::{DatabaseConnection, DatabaseTransaction, DbErr, TransactionTrait};

/// 在事务中执行闭包：`Ok` 提交、`Err` 回滚。
///
/// [`crate::orm::Crud`] 的所有方法都接受 `ConnectionTrait`，事务内可直接复用：
///
/// ```no_run
/// # use core_rs::prelude::*;
/// # mod user {
/// #     use sea_orm::entity::prelude::*;
/// #     #[derive(Clone, Debug, DeriveEntityModel)]
/// #     #[sea_orm(table_name = "users")]
/// #     pub struct Model {
/// #         #[sea_orm(primary_key)]
/// #         pub id: i64,
/// #     }
/// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
/// #     pub enum Relation {}
/// #     impl ActiveModelBehavior for ActiveModel {}
/// # }
/// # async fn demo(db: Db) -> Result<(), sea_orm::DbErr> {
/// core_rs::orm::tx(&db, async |txn| {
///     let total = user::Entity::crud().count(txn).await?;
///     // 更多读写……任一步失败整体回滚
///     Ok(total)
/// })
/// .await?;
/// # Ok(())
/// # }
/// ```
pub async fn tx<T, F>(db: &DatabaseConnection, f: F) -> Result<T, DbErr>
where
    // sugar 形式即 for<'a> HRTB：闭包内的 async 块借用 txn
    F: AsyncFnOnce(&DatabaseTransaction) -> Result<T, DbErr>,
{
    let txn = db.begin().await?;
    match f(&txn).await {
        Ok(value) => txn.commit().await.map(|_| value),
        Err(e) => {
            if let Err(rollback_err) = txn.rollback().await {
                tracing::error!(error = %rollback_err, "transaction rollback failed");
            }
            Err(e)
        }
    }
}
