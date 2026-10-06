//! SeaORM 2 集成（文档 三·5）：连接池、闭包式事务、基础实体约定、统一分页、
//! 游标分页、迁移装配。
//!
//! **实体与仓库不进框架**：entity、repo 全是业务表，留在应用；
//! `migrations/` 目录也在应用侧，框架只提供装配与 migrate 帮手。

pub mod base;
pub mod cursor;
pub mod paginate;
pub mod pool;
pub mod txn;

#[cfg(feature = "migration")]
pub mod migrator;

pub use base::{Crud, CrudExt, PkOf, COL_CREATED_AT, COL_DELETED_AT, COL_UPDATED_AT};
pub use cursor::{CursorPage, CursorQuery};
pub use paginate::{PageParams, Paginated};
pub use pool::connect;
pub use txn::with_txn;
