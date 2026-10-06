//! 游标分页（feed / 大列表场景）：keyset 分页替代 offset——深翻页不退化，
//! 数据插入不影响已读窗口。适合按时间/ID 倒序的信息流。
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # use sea_orm::QueryOrder;
//! # mod video {
//! #     use sea_orm::entity::prelude::*;
//! #     #[derive(Clone, Debug, DeriveEntityModel)]
//! #     #[sea_orm(table_name = "videos")]
//! #     pub struct Model {
//! #         #[sea_orm(primary_key)]
//! #         pub id: i64,
//! #     }
//! #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
//! #     pub enum Relation {}
//! #     impl ActiveModelBehavior for ActiveModel {}
//! # }
//! # async fn demo(db: &DatabaseConnection) -> Result<(), DbErr> {
//! let q: CursorQuery = /* Query<CursorQuery> 提取 */ CursorQuery::default();
//! let page = core_rs::db::cursor::fetch::<video::Entity, _>(
//!     video::Entity::find(), db, video::Column::Id, &q,
//! ).await?;
//! // page.next_cursor 透传给前端，下一页带回 ?cursor=…
//! # Ok(())
//! # }
//! ```

use serde::Deserialize;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

fn default_size() -> u64 {
    20
}

/// 游标分页请求参数（`?cursor=<上页 next_cursor>&size=20`）
#[derive(Debug, Clone, Deserialize)]
pub struct CursorQuery {
    /// 上一页返回的游标；None = 第一页
    #[serde(default)]
    pub cursor: Option<i64>,
    /// 每页条数，限幅 1..=100
    #[serde(default = "default_size")]
    pub size: u64,
}

impl Default for CursorQuery {
    fn default() -> Self {
        Self {
            cursor: None,
            size: default_size(),
        }
    }
}

impl CursorQuery {
    pub fn limit(&self) -> u64 {
        self.size.clamp(1, 100)
    }
}

/// 游标分页结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct CursorPage<T> {
    pub items: Vec<T>,
    /// 下一页游标；None 表示没有更多数据
    pub next_cursor: Option<i64>,
}

/// 按整数列（通常是主键 id）倒序的 keyset 分页。
/// 返回 `size + 1` 条探测是否还有下一页，多出的一条不进 items。
pub async fn fetch<E, C>(
    select: sea_orm::Select<E>,
    db: &C,
    cursor_col: E::Column,
    q: &CursorQuery,
) -> Result<CursorPage<E::Model>, DbErr>
where
    E: EntityTrait,
    E::Model: Send + Sync,
    C: ConnectionTrait,
{
    let limit = q.limit();
    let sel = select
        .filter(cursor_col.lt(q.cursor.unwrap_or(i64::MAX)))
        .order_by_desc(cursor_col)
        .limit(limit + 1);

    let rows: Vec<E::Model> = sel.all(db).await?;
    let mut items = rows;
    let next_cursor = if items.len() as u64 > limit {
        items.truncate(limit as usize);
        // 游标取本页最后一条的列值（整数主键约定；uuid 主键请自写提取）
        let last = items.last().ok_or_else(|| {
            DbErr::Custom("cursor pagination: empty page but has more".to_string())
        })?;
        let v = <E::Model as sea_orm::ModelTrait>::get(last, cursor_col);
        Some(value_to_i64(v)?)
    } else {
        None
    };
    Ok(CursorPage { items, next_cursor })
}

fn value_to_i64(v: sea_orm::Value) -> Result<i64, DbErr> {
    match v {
        sea_orm::Value::TinyInt(Some(v)) => Ok(v as i64),
        sea_orm::Value::SmallInt(Some(v)) => Ok(v as i64),
        sea_orm::Value::Int(Some(v)) => Ok(v as i64),
        sea_orm::Value::BigInt(Some(v)) => Ok(v),
        other => Err(DbErr::Custom(format!(
            "cursor pagination requires an integer column, got {other:?}"
        ))),
    }
}
