//! SeaORM 2 集成（文档 三·5）：连接池、闭包式事务、基础实体约定、统一分页、
//! 游标分页、迁移装配。
//!
//! **实体与仓库不进框架**：entity、repo 全是业务表，留在应用；
//! `migrations/` 目录也在应用侧，框架只提供装配与 migrate 帮手。

pub mod base; // 声明 base 子模块：基础实体约定与泛型 CRUD
pub mod cursor; // 声明 cursor 子模块：keyset 游标分页
pub mod paginate; // 声明 paginate 子模块：统一 offset 分页
pub mod pool; // 声明 pool 子模块：连接池构建
pub mod txn; // 声明 txn 子模块：闭包式事务与 savepoint

#[cfg(feature = "migration")] // 仅在开启 migration feature 时编译下一行
pub mod migrator; // 声明 migrator 子模块：迁移装配与执行

pub use base::{Crud, CrudExt, PkOf, COL_CREATED_AT, COL_DELETED_AT, COL_UPDATED_AT}; // 重导出基础 CRUD 类型与审计/软删列名常量
pub use cursor::{CursorPage, CursorQuery}; // 重导出游标分页的请求与结果类型
pub use paginate::{PageParams, Paginated}; // 重导出统一分页的参数与结果类型
pub use pool::connect; // 重导出连接池构建函数
pub use txn::with_txn; // 重导出闭包式事务函数
