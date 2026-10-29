//! 迁移脚本目录。
//!
//! aurora 自身**不定义业务表**，因此本目录不含迁移脚本；应用侧在自己的 crate 中
//! 声明 `Migrator`（见 `docs/10` §3）。此处集中 re-export 编写迁移所需的类型：
//!
//! ```ignore
//! use aurora::db::migrations::*;
//!
//! pub struct Migrator;
//!
//! impl MigratorTrait for Migrator {
//!     fn migrations() -> Vec<Box<dyn MigrationTrait>> {
//!         vec![Box::new(m20260101_000001_create_users::Migration)]
//!     }
//! }
//! ```
//!
//! 文件名约定：`m<UTC 时间戳>_<动作>_<对象>.rs`（动作：`create` / `add` / `drop` /
//! `alter` / `rename` / `seed`），时间戳唯一且递增；**已应用的迁移不可修改**。

pub use sea_orm_migration::prelude::*;
