//! 闭包式事务，替代手写 begin/commit（文档 三·5）：成功提交、任一步 Err 自动回滚。
//! 嵌套事务走 savepoint（sea-orm `TransactionTrait::begin_nested`）。

use sea_orm::{ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbErr, Statement, TransactionTrait}; // 引入连接/连接池/事务/错误/原生语句与事务 trait

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
pub async fn with_txn<T, F>(db: &DatabaseConnection, f: F) -> Result<T, DbErr> // 在事务中执行闭包，成功提交失败回滚
where // 泛型约束开始
    // sugar 形式即 for<'a> HRTB：闭包内的 async 块借用 txn
    F: AsyncFnOnce(&DatabaseTransaction) -> Result<T, DbErr>, // 闭包签名：接收事务引用返回结果
{
    let txn = db.begin().await?; // 开启事务
    match f(&txn).await { // 执行闭包并匹配结果
        Ok(value) => txn.commit().await.map(|_| value), // 成功则提交并返回闭包值
        Err(e) => { // 闭包返回错误
            if let Err(rollback_err) = txn.rollback().await { // 尝试回滚
                tracing::error!(error = %rollback_err, "transaction rollback failed"); // 回滚失败记录错误日志
            }
            Err(e) // 返回原始错误
        }
    }
}

/// SAVEPOINT 名护栏：`[A-Za-z_][A-Za-z0-9_]{0,63}`。
/// SAVEPOINT 名无法绑定参数，历史上靠注释约定"勿传用户输入"——现在用代码强制，
/// 拼接即报错，而不是把注入面留给下一个不看文档的调用方。
fn validate_savepoint_name(name: &str) -> Result<(), DbErr> { // 校验 SAVEPOINT 名是否为合法标识符
    let ok = !name.is_empty() // 名称非空
        && name.len() <= 64 // 长度不超过 64
        && !name.starts_with(|c: char| c.is_ascii_digit()) // 首字符不能是数字
        && name // 其余字符
            .chars() // 遍历字符
            .all(|c| c.is_ascii_alphanumeric() || c == '_'); // 只允许字母数字与下划线
    if ok { // 合法时
        Ok(()) // 返回成功
    } else {
        Err(DbErr::Custom(format!( // 非法时返回自定义错误
            "invalid savepoint name {name:?}: expected [A-Za-z_][A-Za-z0-9_]{{0,63}} (SAVEPOINT names cannot be parameterized)" // 错误信息：说明合法格式
        )))
    }
}

/// 嵌套事务（savepoint）：已有事务句柄内部再开一层，失败只回滚到 savepoint。
/// `name` 必须是合法标识符（内部已校验，非标识符直接报错）。sea-orm 2 无内建
/// savepoint，走原生 SQL。
pub async fn with_savepoint<T, F>( // 嵌套事务（savepoint）执行函数
    txn: &DatabaseTransaction, // 外层事务句柄
    name: &str, // savepoint 名
    f: F, // 待执行闭包
) -> Result<T, DbErr> // 返回闭包结果或错误
where // 泛型约束开始
    F: AsyncFnOnce(&DatabaseTransaction) -> Result<T, DbErr>, // 闭包签名
{
    validate_savepoint_name(name)?; // 先校验 savepoint 名
    let backend = txn.get_database_backend(); // 取数据库后端类型
    let exec = |sql: String| txn.execute_raw(Statement::from_string(backend, sql)); // 封装原生 SQL 执行闭包
    exec(format!("SAVEPOINT {name}")).await?; // 创建 savepoint
    match f(txn).await { // 执行闭包
        Ok(value) => { // 成功
            exec(format!("RELEASE SAVEPOINT {name}")).await?; // 释放 savepoint
            Ok(value) // 返回闭包值
        }
        Err(e) => { // 失败
            if let Err(rollback_err) = exec(format!("ROLLBACK TO SAVEPOINT {name}")).await { // 回滚到 savepoint
                tracing::error!(error = %rollback_err, "savepoint rollback failed"); // 回滚失败记录日志
            }
            Err(e) // 返回原始错误
        }
    }
}
