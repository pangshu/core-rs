//! 基础实体约定：审计字段 `created_at` / `updated_at`、软删 `deleted_at`（文档 三·5）。
//!
//! ## 约定（存在对应列即自动生效，不存在的列完全不影响）
//!
//! - `created_at`：insert 时若未显式赋值，自动填当前时间；
//! - `updated_at`：insert / update 时若未显式赋值，自动填当前时间；
//! - `deleted_at`：`DateTimeUtc` 软删标记（NULL = 存活）——`get`/`list`/`page`/`count`
//!   自动过滤已删行，`delete` 转为置位更新（无该列的实体则执行物理删除）。
//!
//! 时间戳只对 `Timestamp` / `TimestampWithTimeZone` 类型列生效，值类型与
//! 实体字段类型（`DateTime` / `DateTimeUtc`）一一对应，不会产生类型错配。
//!
//! 用法：`Entity::crud().page(&db, &q).await`。需要绕过约定时直接用
//! sea-orm 原生 `Entity::find()` / `ActiveModel` 即可。

use std::marker::PhantomData;
use std::str::FromStr;

use chrono::Utc;
use sea_orm::sea_query::IntoValueTuple;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, Iterable,
    IntoActiveModel, PaginatorTrait, PrimaryKeyToColumn, PrimaryKeyTrait, QueryFilter, Value,
};

use super::paginate::{fetch_paginated, PageParams, Paginated};

/// 自动填充：创建时间列名
pub const COL_CREATED_AT: &str = "created_at";
/// 自动填充：更新时间列名
pub const COL_UPDATED_AT: &str = "updated_at";
/// 软删：标记列名（DateTimeUtc，NULL = 存活）
pub const COL_DELETED_AT: &str = "deleted_at";

/// 实体主键值类型（单列主键）
pub type PkOf<E> = <<E as EntityTrait>::PrimaryKey as PrimaryKeyTrait>::ValueType;

/// 泛型 CRUD 服务，通过 [`CrudExt::crud`] 获得
pub struct Crud<E: EntityTrait>(PhantomData<fn() -> E>);

impl<E: EntityTrait> Default for Crud<E> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// 为所有实体提供 `Entity::crud()` 入口
pub trait CrudExt: EntityTrait + Sized {
    fn crud() -> Crud<Self> {
        Crud(PhantomData)
    }
}

impl<E: EntityTrait> CrudExt for E {}

impl<E> Crud<E>
where
    E: EntityTrait,
    E::Model: Send + Sync + IntoActiveModel<E::ActiveModel>,
    E::ActiveModel: ActiveModelTrait + Send + Sync,
{
    // ---------- 约定列查找 ----------

    fn column(name: &str) -> Option<E::Column> {
        E::Column::from_str(name).ok()
    }

    fn created_at_column() -> Option<E::Column> {
        Self::column(COL_CREATED_AT)
    }

    fn updated_at_column() -> Option<E::Column> {
        Self::column(COL_UPDATED_AT)
    }

    fn deleted_at_column() -> Option<E::Column> {
        Self::column(COL_DELETED_AT)
    }

    /// 按列声明的类型构造"当前时间"值，类型不匹配的列返回 None（不填充）
    fn now_value(col: E::Column) -> Option<Value> {
        let now = Utc::now();
        match col.def().get_column_type() {
            sea_orm::ColumnType::Timestamp | sea_orm::ColumnType::DateTime => {
                Some(Value::ChronoDateTime(Some(now.naive_utc())))
            }
            sea_orm::ColumnType::TimestampWithTimeZone => {
                Some(Value::ChronoDateTimeUtc(Some(now)))
            }
            _ => None,
        }
    }

    /// 软删过滤条件（实体无 deleted_at 列时返回 None）
    fn not_deleted_condition() -> Option<Condition> {
        Self::deleted_at_column().map(|col| Condition::all().add(col.is_null()))
    }

    /// 基础查询：带软删过滤
    fn select() -> sea_orm::Select<E> {
        let mut sel = E::find();
        if let Some(cond) = Self::not_deleted_condition() {
            sel = sel.filter(cond);
        }
        sel
    }

    fn single_pk_value(id: PkOf<E>) -> Result<(E::Column, Value), DbErr> {
        let value = match id.into_value_tuple() {
            sea_orm::sea_query::ValueTuple::One(v) => v,
            _ => {
                return Err(DbErr::Custom(
                    "Crud 仅支持单列主键，复合主键请直接使用 sea-orm 原生 API".to_string(),
                ))
            }
        };
        let col = <E::PrimaryKey as Iterable>::iter()
            .next()
            .map(|pk| pk.into_column())
            .ok_or_else(|| DbErr::Custom("实体没有主键".to_string()))?;
        Ok((col, value))
    }

    // ---------- 写 ----------

    /// 插入。自动填充 created_at / updated_at（列存在且未显式赋值时）。
    pub async fn insert<C: ConnectionTrait>(
        &self,
        db: &C,
        mut am: E::ActiveModel,
    ) -> Result<E::Model, DbErr> {
        for col in [Self::created_at_column(), Self::updated_at_column()]
            .into_iter()
            .flatten()
        {
            if am.is_not_set(col) {
                if let Some(v) = Self::now_value(col) {
                    am.set(col, v);
                }
            }
        }
        am.insert(db).await
    }

    /// 按主键更新。自动填充 updated_at（列存在且未显式赋值时）。
    /// 带 deleted_at 列的实体不允许修改已软删的行：软删保护用**单语句条件
    /// 更新**（先 SELECT 再 UPDATE 的两步之间存在窗口，并发软删后仍会改到
    /// 已删行）；命中 0 行返回 `DbErr::RecordNotFound`。复合主键或主键未赋值时
    /// 退回「先查后改」（尽力而为）。
    pub async fn update<C: ConnectionTrait>(
        &self,
        db: &C,
        mut am: E::ActiveModel,
    ) -> Result<E::Model, DbErr> {
        if let Some(col) = Self::updated_at_column() {
            if am.is_not_set(col) {
                if let Some(v) = Self::now_value(col) {
                    am.set(col, v);
                }
            }
        }
        if let (Some(del_col), Some(sea_orm::sea_query::ValueTuple::One(v))) =
            (Self::deleted_at_column(), am.get_primary_key_value())
        {
            if let Some(pk_col) = <E::PrimaryKey as Iterable>::iter()
                .next()
                .map(|pk| pk.into_column())
            {
                let res = E::update_many()
                    .filter(pk_col.eq(v.clone()))
                    .filter(del_col.is_null())
                    .set(am)
                    .exec(db)
                    .await?;
                if res.rows_affected == 0 {
                    return Err(DbErr::RecordNotFound(
                        "row not found or logically deleted".to_string(),
                    ));
                }
                // 条件 update 不回传行：按主键回查（get 语义，已过滤软删行）
                return Self::select()
                    .filter(pk_col.eq(v))
                    .one(db)
                    .await?
                    .ok_or_else(|| {
                        DbErr::RecordNotFound("row not found or logically deleted".to_string())
                    });
            }
        }
        if Self::deleted_at_column().is_some() {
            Self::ensure_alive(db, &am).await?;
        }
        am.update(db).await
    }

    /// 单列主键且主键值已提供时，校验行未被软删；其余情况跳过校验，
    /// 交由 sea-orm 原生行为（行不存在时报 RecordNotFound / RecordNotUpdated）。
    async fn ensure_alive<C: ConnectionTrait>(db: &C, am: &E::ActiveModel) -> Result<(), DbErr> {
        let Some(sea_orm::sea_query::ValueTuple::One(v)) = am.get_primary_key_value() else {
            return Ok(());
        };
        let Some(pk_col) = <E::PrimaryKey as Iterable>::iter()
            .next()
            .map(|pk| pk.into_column())
        else {
            return Ok(());
        };
        if Self::select().filter(pk_col.eq(v)).one(db).await?.is_none() {
            return Err(DbErr::RecordNotFound(
                "row not found or logically deleted".to_string(),
            ));
        }
        Ok(())
    }

    /// 删除。有 deleted_at 列时执行软删（同时填充 updated_at），否则物理删除。
    /// 返回是否实际影响到行。
    pub async fn delete<C: ConnectionTrait>(&self, db: &C, id: PkOf<E>) -> Result<bool, DbErr> {
        let (pk_col, pk_value) = Self::single_pk_value(id)?;

        match Self::deleted_at_column() {
            Some(col) => {
                // deleted_at 须为时间类型列（Timestamp / TimestampWithTimeZone）
                let deleted_value = Self::now_value(col).ok_or_else(|| {
                    DbErr::Custom("deleted_at column must be a timestamp type".to_string())
                })?;
                let mut upd = E::update_many()
                    .col_expr(col, sea_orm::sea_query::Expr::value(deleted_value));
                if let Some(c) = Self::updated_at_column() {
                    if let Some(v) = Self::now_value(c) {
                        upd = upd.col_expr(c, sea_orm::sea_query::Expr::value(v));
                    }
                }
                // 只作用于存活行：对已删行重复 delete 返回 false（语义等同 get 返回 None）
                let res = upd
                    .filter(pk_col.eq(pk_value))
                    .filter(col.is_null())
                    .exec(db)
                    .await?;
                Ok(res.rows_affected > 0)
            }
            None => {
                let res = E::delete_many().filter(pk_col.eq(pk_value)).exec(db).await?;
                Ok(res.rows_affected > 0)
            }
        }
    }

    // ---------- 读 ----------

    /// 按主键查询（自动过滤已删行）
    pub async fn get<C: ConnectionTrait>(
        &self,
        db: &C,
        id: PkOf<E>,
    ) -> Result<Option<E::Model>, DbErr> {
        let mut sel = E::find_by_id(id);
        if let Some(cond) = Self::not_deleted_condition() {
            sel = sel.filter(cond);
        }
        sel.one(db).await
    }

    /// 全量列表（自动过滤已删行）
    pub async fn list<C: ConnectionTrait>(&self, db: &C) -> Result<Vec<E::Model>, DbErr> {
        Self::select().all(db).await
    }

    /// 计数（自动过滤已删行）
    pub async fn count<C: ConnectionTrait>(&self, db: &C) -> Result<u64, DbErr> {
        Self::select().count(db).await
    }

    /// 分页查询（自动过滤已删行；参数规范化见 [`PageParams`]）
    pub async fn page<C: ConnectionTrait>(
        &self,
        db: &C,
        q: &PageParams,
    ) -> Result<Paginated<E::Model>, DbErr> {
        fetch_paginated(Self::select(), db, q).await
    }
}
