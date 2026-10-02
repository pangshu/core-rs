//! ORM 层封装：连接池初始化、`Db` 提取器、泛型 CRUD、统一分页、通用搜索、事务封装、迁移集成。

pub mod crud;
pub mod page;
pub mod pool;
pub mod search;
pub mod tx;
#[cfg(feature = "migration")]
pub mod migrate;

pub use crud::{Crud, CrudExt, PkOf};
pub use pool::Db;
pub use search::{SearchApply, SearchQuery};
pub use tx::tx;
