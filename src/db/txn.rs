//! 闭包式事务，替代手写 begin/commit（文档 三·5）：成功提交、任一步 Err 自动回滚。
//! 嵌套事务走 savepoint（sea-orm `TransactionTrait::begin_nested`）。

use sea_orm::{ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbErr, Statement, TransactionTrait};

/// 在事务中执行闭包：`Ok` 提交、`Err` 回滚。闭包内拿到 `&DatabaseTransaction`，
/// sea-orm 的查询都接受 `&impl ConnectionTrait`，事务内可直接复用业务查询函数。
///
/// ```no_run
/// # use core_rs::prelude::*;
/// # use sea_orm::TransactionTrait;
/// # async fn demo(db: &DatabaseConnection) -> Result<(), DbErr> {
/// core_rs::db::with_txn(db, async move |txn| {
///     // 多表写入……任一步返回 Err 自动回滚（async 闭包 sugar 才满足 AsyncFnOnce）
///     Ok(())
/// })
/// .await?;
/// # Ok(())
/// # }
/// ```
pub async fn with_txn<T, F>(db: &DatabaseConnection, f: F) -> Result<T, DbErr>
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

/// SAVEPOINT 名护栏：`[A-Za-z_][A-Za-z0-9_]{0,63}`。
/// SAVEPOINT 名无法绑定参数，历史上靠注释约定"勿传用户输入"——现在用代码强制，
/// 拼接即报错，而不是把注入面留给下一个不看文档的调用方。
fn validate_savepoint_name(name: &str) -> Result<(), DbErr> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(DbErr::Custom(format!(
            "invalid savepoint name {name:?}: expected [A-Za-z_][A-Za-z0-9_]{{0,63}} (SAVEPOINT names cannot be parameterized)"
        )))
    }
}

/// 嵌套事务（savepoint）：已有事务句柄内部再开一层，失败只回滚到 savepoint。
/// `name` 必须是合法标识符（内部已校验，非标识符直接报错）。sea-orm 2 无内建
/// savepoint，走原生 SQL。
pub async fn with_savepoint<T, F>(
    txn: &DatabaseTransaction,
    name: &str,
    f: F,
) -> Result<T, DbErr>
where
    F: AsyncFnOnce(&DatabaseTransaction) -> Result<T, DbErr>,
{
    validate_savepoint_name(name)?;
    let backend = txn.get_database_backend();
    let exec = |sql: String| txn.execute_raw(Statement::from_string(backend, sql));
    exec(format!("SAVEPOINT {name}")).await?;
    match f(txn).await {
        Ok(value) => {
            exec(format!("RELEASE SAVEPOINT {name}")).await?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rollback_err) = exec(format!("ROLLBACK TO SAVEPOINT {name}")).await {
                tracing::error!(error = %rollback_err, "savepoint rollback failed");
            }
            Err(e)
        }
    }
}
