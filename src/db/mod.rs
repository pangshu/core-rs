//! 数据库模块（feature `db`）。
//!
//! 基于 **SeaORM 2**（见 `docs/10`）：连接池、迁移入口、事务助手、游标分页、
//! 流式读取与 [`Repository`] 抽象。**业务实体由业务侧声明**；框架不内置 CRUD /
//! 软删 / 审计。
//!
//! 驱动由 `db-postgres` / `db-sqlite` / `db-mysql` 三选一（互斥）。

pub mod cursor;
mod error;
pub mod migration;
pub mod migrations;
pub mod pool;
pub mod repository;
pub mod stream;
pub mod txn;

pub use cursor::{Cursor, paginate};
pub use migration::{MigratorTrait, down, migrate_on_start, status, up};
pub use pool::{DbHealth, DbPool, connect, ping};
pub use repository::{Repository, require_found};
pub use stream::{StreamExt, stream as stream_all};
pub use txn::{is_serialization_failure, transaction, transaction_with_retry};

/// SeaORM 重导出（业务侧声明实体 / 查询时使用）。
pub use sea_orm;
