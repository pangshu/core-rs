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

use serde::Deserialize; // 引入反序列化派生，用于游标请求参数
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect}; // 引入列/连接/错误/实体与过滤排序限制等 trait

fn default_size() -> u64 { // size 的默认值函数
    20 // 默认每页 20 条
}

/// 游标分页请求参数（`?cursor=<上页 next_cursor>&size=20`）
#[derive(Debug, Clone, Deserialize)] // 派生调试/克隆/反序列化
pub struct CursorQuery { // 游标请求参数结构体
    /// 上一页返回的游标；None = 第一页
    #[serde(default)] // 缺省为 None
    pub cursor: Option<i64>, // 游标值（通常是主键），None 表示第一页
    /// 每页条数，限幅 1..=100
    #[serde(default = "default_size")] // 缺省时用 default_size 填充
    pub size: u64, // 每页条数
}

impl Default for CursorQuery { // 为 CursorQuery 实现 Default
    fn default() -> Self { // 默认构造
        Self { // 构造结构体
            cursor: None, // 无游标（第一页）
            size: default_size(), // 默认每页条数
        }
    }
}

impl CursorQuery { // CursorQuery 的方法
    pub fn limit(&self) -> u64 { // 计算实际 LIMIT
        self.size.clamp(1, 100) // 把条数限制在 1..=100
    }
}

/// 游标分页结果
#[derive(Debug, Clone, serde::Serialize)] // 派生调试/克隆/序列化
pub struct CursorPage<T> { // 游标分页结果结构体
    pub items: Vec<T>, // 当前页记录
    /// 下一页游标；None 表示没有更多数据
    pub next_cursor: Option<i64>, // 下一页游标
}

/// 按整数列（通常是主键 id）倒序的 keyset 分页。
/// 返回 `size + 1` 条探测是否还有下一页，多出的一条不进 items。
pub async fn fetch<E, C>( // 游标分页执行函数
    select: sea_orm::Select<E>, // 待分页的查询
    db: &C, // 数据库连接
    cursor_col: E::Column, // 作为游标的整数列
    q: &CursorQuery, // 游标请求参数
) -> Result<CursorPage<E::Model>, DbErr> // 返回游标分页结果或错误
where // 泛型约束开始
    E: EntityTrait, // 约束：E 为实体
    E::Model: Send + Sync, // 约束：模型可跨线程
    C: ConnectionTrait, // 约束：C 为连接
{
    let limit = q.limit(); // 规范化后的每页条数
    let sel = select // 构造 keyset 查询
        .filter(cursor_col.lt(q.cursor.unwrap_or(i64::MAX))) // 只取小于游标的行（首屏用 i64::MAX）
        .order_by_desc(cursor_col) // 按游标列倒序
        .limit(limit + 1); // 多取一条用于判断是否还有下一页

    let rows: Vec<E::Model> = sel.all(db).await?; // 执行查询取出记录
    let mut items = rows; // 转为可变记录集
    let next_cursor = if items.len() as u64 > limit { // 超过 limit 说明还有下一页
        items.truncate(limit as usize); // 截断掉多出的探测记录
        // 游标取本页最后一条的列值（整数主键约定；uuid 主键请自写提取）
        let last = items.last().ok_or_else(|| { // 取本页最后一条作为游标来源
            DbErr::Custom("cursor pagination: empty page but has more".to_string()) // 空页却有更多数据，属异常
        })?;
        let v = <E::Model as sea_orm::ModelTrait>::get(last, cursor_col); // 读出该行的游标列值
        Some(value_to_i64(v)?) // 转为 i64 作为下一页游标
    } else {
        None // 没有更多数据，游标为 None
    };
    Ok(CursorPage { items, next_cursor }) // 组装游标分页结果
}

fn value_to_i64(v: sea_orm::Value) -> Result<i64, DbErr> { // 把数据库值转成 i64 游标
    match v { // 按值类型匹配
        sea_orm::Value::TinyInt(Some(v)) => Ok(v as i64), // 8 位整数转 i64
        sea_orm::Value::SmallInt(Some(v)) => Ok(v as i64), // 16 位整数转 i64
        sea_orm::Value::Int(Some(v)) => Ok(v as i64), // 32 位整数转 i64
        sea_orm::Value::BigInt(Some(v)) => Ok(v), // 64 位整数直接返回
        other => Err(DbErr::Custom(format!( // 其他类型不支持，返回错误
            "cursor pagination requires an integer column, got {other:?}" // 错误信息：需要整数列
        ))),
    }
}
