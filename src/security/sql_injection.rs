//! SQL 防注入（文档 三·16）：SeaORM 默认参数化，本模块负责**约定与检查**——
//! 提供安全的 raw 封装、禁止字符串拼 SQL 的审查点。
//!
//! ## 约定（审查点）
//!
//! 1. 业务代码一律用 SeaORM 查询构造器（参数化，天然安全）；
//! 2. 确需 raw SQL（复杂报表等）时，**只能**经本模块的
//!    [`safe_statement`] / [`safe_select_all`] —— 值走绑定参数，
//!    表名/列名等标识符只允许来自常量或白名单（见 [`allowlisted_ident`]）；
//! 3. 代码评审红线：`format!` / `+` 拼接 SQL 字符串一票否决
//!    （`grep -rn 'Statement::from_string' src/` 应只出现在本模块）。

use sea_orm::{ConnectionTrait, DbErr, Statement, Value}; // 引入 SeaORM 连接 trait、错误、语句与绑定值类型

/// 构造参数化语句：`sql` 中的占位符（`?` / `$1..$n`，随方言）与 `params`
/// 一一对应。这是框架内**唯一**推荐的 raw SQL 入口（后端方言自动跟随连接）。
pub fn safe_statement<C: ConnectionTrait>(db: &C, sql: &str, params: Vec<Value>) -> Statement { // 用连接后端方言把 sql 与参数绑定成参数化 Statement
    Statement::from_sql_and_values(db.get_database_backend(), sql, params) // 按后端生成占位符风格的参数化语句（值走绑定，杜绝拼接）
}

/// 白名单校验标识符（表名 / 列名 / 排序方向等动态片段的护栏）。
/// 片段必须与白名单完全一致（区分大小写），否则报错——调用方把用户输入
/// 映射到白名单常量后再传入。
pub fn allowlisted_ident(ident: &str, allowlist: &[&str]) -> Result<String, DbErr> { // 校验动态标识符是否在白名单内
    if allowlist.contains(&ident) { // 命中白名单（区分大小写精确匹配）才放行
        Ok(ident.to_string()) // 返回通过校验的标识符字符串
    } else { // 未命中白名单，拒绝拼接
        Err(DbErr::Custom(format!( // 构造自定义数据库错误返回
            "SQL identifier `{ident}` not in allowlist {allowlist:?}（防注入审查点：禁止拼接未校验标识符）"
        )))
    }
}

/// 执行参数化 raw 查询（多行）
pub async fn safe_select_all<C: ConnectionTrait>( // 多行 raw 查询的安全封装
    db: &C, // 数据库连接（决定后端方言）
    sql: &str, // 参数化 SQL 文本
    params: Vec<Value>, // 与占位符一一对应的绑定值
) -> Result<Vec<sea_orm::QueryResult>, DbErr> { // 返回全部结果行或数据库错误
    db.query_all_raw(safe_statement(db, sql, params)).await // 用安全语句执行 query_all_raw
}

/// 执行参数化 raw 查询（单行）
pub async fn safe_select_one<C: ConnectionTrait>( // 单行 raw 查询的安全封装
    db: &C, // 数据库连接（决定后端方言）
    sql: &str, // 参数化 SQL 文本
    params: Vec<Value>, // 与占位符一一对应的绑定值
) -> Result<Option<sea_orm::QueryResult>, DbErr> { // 返回至多一行结果或数据库错误
    db.query_one_raw(safe_statement(db, sql, params)).await // 用安全语句执行 query_one_raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_rejects_unlisted() {
        let cols = ["id", "name", "created_at"];
        assert!(allowlisted_ident("name", &cols).is_ok());
        assert!(allowlisted_ident("name; DROP TABLE users", &cols).is_err());
        assert!(allowlisted_ident("1=1", &cols).is_err());
    }
}
