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

use std::marker::PhantomData; // 引入零大小标记类型 PhantomData，承载泛型实体参数
use std::str::FromStr; // 引入 FromStr trait，用于按字符串解析列

use chrono::Utc; // 引入 UTC 当前时间工具
use sea_orm::sea_query::IntoValueTuple; // 引入主键值转元组的 trait
use sea_orm::{ // 引入 SeaORM 常用 trait 集合
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, Iterable, // ActiveModel/列/条件/连接/错误/实体/可迭代等 trait
    IntoActiveModel, PaginatorTrait, PrimaryKeyToColumn, PrimaryKeyTrait, QueryFilter, Value, // 转 ActiveModel/分页/主键列/主键/查询过滤/值类型等
};

use super::paginate::{fetch_paginated, PageParams, Paginated}; // 引入分页执行函数与参数/结果类型

/// 自动填充：创建时间列名
pub const COL_CREATED_AT: &str = "created_at"; // 创建时间列名常量
/// 自动填充：更新时间列名
pub const COL_UPDATED_AT: &str = "updated_at"; // 更新时间列名常量
/// 软删：标记列名（DateTimeUtc，NULL = 存活）
pub const COL_DELETED_AT: &str = "deleted_at"; // 软删标记列名常量

/// 实体主键值类型（单列主键）
pub type PkOf<E> = <<E as EntityTrait>::PrimaryKey as PrimaryKeyTrait>::ValueType; // 提取实体单列主键的值类型别名

/// 泛型 CRUD 服务，通过 [`CrudExt::crud`] 获得
pub struct Crud<E: EntityTrait>(PhantomData<fn() -> E>); // 用 PhantomData 承载实体类型参数的空结构体

impl<E: EntityTrait> Default for Crud<E> { // 为 Crud 实现 Default
    fn default() -> Self { // 默认构造
        Self(PhantomData) // 返回零大小的 Crud 实例
    }
}

/// 为所有实体提供 `Entity::crud()` 入口
pub trait CrudExt: EntityTrait + Sized { // 扩展 trait：为所有实体提供 crud 入口
    fn crud() -> Crud<Self> { // 静态方法返回本实体的 CRUD 服务
        Crud(PhantomData) // 构造 CRUD 服务实例
    }
}

impl<E: EntityTrait> CrudExt for E {} // 为所有实体类型自动实现 CrudExt

impl<E> Crud<E> // 为 Crud<E> 实现通用方法
where // 泛型约束开始
    E: EntityTrait, // 约束：E 必须是实体
    E::Model: Send + Sync + IntoActiveModel<E::ActiveModel>, // 约束：模型可跨线程且可转 ActiveModel
    E::ActiveModel: ActiveModelTrait + Send + Sync, // 约束：ActiveModel 可写且可跨线程
{
    // ---------- 约定列查找 ----------

    fn column(name: &str) -> Option<E::Column> { // 按列名字符串查找实体列
        E::Column::from_str(name).ok() // 解析成功返回列，失败返回 None
    }

    fn created_at_column() -> Option<E::Column> { // 查找 created_at 列
        Self::column(COL_CREATED_AT) // 按常量名查找
    }

    fn updated_at_column() -> Option<E::Column> { // 查找 updated_at 列
        Self::column(COL_UPDATED_AT) // 按常量名查找
    }

    fn deleted_at_column() -> Option<E::Column> { // 查找 deleted_at 列
        Self::column(COL_DELETED_AT) // 按常量名查找
    }

    /// 按列声明的类型构造"当前时间"值，类型不匹配的列返回 None（不填充）
    fn now_value(col: E::Column) -> Option<Value> { // 按列类型生成当前时间值
        let now = Utc::now(); // 取当前 UTC 时间
        match col.def().get_column_type() { // 匹配列的数据库类型
            sea_orm::ColumnType::Timestamp | sea_orm::ColumnType::DateTime => { // 无时区时间戳/日期时间类型
                Some(Value::ChronoDateTime(Some(now.naive_utc()))) // 转为无时区的 NaiveDateTime 值
            }
            sea_orm::ColumnType::TimestampWithTimeZone => { // 带时区时间戳类型
                Some(Value::ChronoDateTimeUtc(Some(now))) // 直接使用带 UTC 的时间值
            }
            _ => None, // 其他类型不填充，返回 None
        }
    }

    /// 软删过滤条件（实体无 deleted_at 列时返回 None）
    fn not_deleted_condition() -> Option<Condition> { // 构造"未软删"过滤条件
        Self::deleted_at_column().map(|col| Condition::all().add(col.is_null())) // deleted_at 为 NULL 视为存活
    }

    /// 基础查询：带软删过滤
    fn select() -> sea_orm::Select<E> { // 构造带软删过滤的基础查询
        let mut sel = E::find(); // 从实体的 find 查询起步
        if let Some(cond) = Self::not_deleted_condition() { // 存在软删列时
            sel = sel.filter(cond); // 追加"未删除"过滤条件
        }
        sel // 返回构造好的查询
    }

    fn single_pk_value(id: PkOf<E>) -> Result<(E::Column, Value), DbErr> { // 把单列主键值拆成(列,值)，复合主键报错
        let value = match id.into_value_tuple() { // 将主键值转为值元组后匹配
            sea_orm::sea_query::ValueTuple::One(v) => v, // 单列主键：取出唯一的值
            _ => { // 复合主键或其他情况
                return Err(DbErr::Custom( // 返回自定义错误
                    "Crud 仅支持单列主键，复合主键请直接使用 sea-orm 原生 API".to_string(), // 错误信息：仅支持单列主键
                ))
            }
        };
        let col = <E::PrimaryKey as Iterable>::iter() // 遍历主键定义
            .next() // 取首个主键列
            .map(|pk| pk.into_column()) // 转为列枚举
            .ok_or_else(|| DbErr::Custom("实体没有主键".to_string()))?; // 无主键则报错
        Ok((col, value)) // 返回主键列与值
    }

    // ---------- 写 ----------

    /// 插入。自动填充 created_at / updated_at（列存在且未显式赋值时）。
    pub async fn insert<C: ConnectionTrait>( // 插入一行并返回完整模型
        &self, // 不可变借用 CRUD 服务
        db: &C, // 任意连接（连接池或事务）
        mut am: E::ActiveModel, // 待插入的 ActiveModel，可变以便填充字段
    ) -> Result<E::Model, DbErr> { // 返回插入后的模型或错误
        for col in [Self::created_at_column(), Self::updated_at_column()] // 遍历 created_at/updated_at 两个约定列
            .into_iter() // 转为迭代器
            .flatten() // 过滤掉不存在的列
        {
            if am.is_not_set(col) { // 该列未被显式赋值时
                if let Some(v) = Self::now_value(col) { // 且列类型支持当前时间时
                    am.set(col, v); // 自动填充当前时间
                }
            }
        }
        am.insert(db).await // 执行插入并返回模型
    }

    /// 按主键更新。自动填充 updated_at（列存在且未显式赋值时）。
    /// 带 deleted_at 列的实体不允许修改已软删的行：软删保护用**单语句条件
    /// 更新**（先 SELECT 再 UPDATE 的两步之间存在窗口，并发软删后仍会改到
    /// 已删行）；命中 0 行返回 `DbErr::RecordNotFound`。复合主键或主键未赋值时
    /// 退回「先查后改」（尽力而为）。
    pub async fn update<C: ConnectionTrait>( // 按主键更新并返回更新后模型
        &self, // 不可变借用 CRUD 服务
        db: &C, // 任意连接
        mut am: E::ActiveModel, // 待更新的 ActiveModel，可变以便填充字段
    ) -> Result<E::Model, DbErr> { // 返回更新后的模型或错误
        if let Some(col) = Self::updated_at_column() { // 存在 updated_at 列时
            if am.is_not_set(col) { // 未显式赋值时
                if let Some(v) = Self::now_value(col) { // 列类型支持时间时
                    am.set(col, v); // 自动填充更新时间
                }
            }
        }
        if let (Some(del_col), Some(sea_orm::sea_query::ValueTuple::One(v))) = // 同时满足：有软删列且主键为单列已赋值
            (Self::deleted_at_column(), am.get_primary_key_value()) // 取出软删列与主键值
        {
            if let Some(pk_col) = <E::PrimaryKey as Iterable>::iter() // 取主键列
                .next() // 首个主键列
                .map(|pk| pk.into_column()) // 转为列枚举
            {
                let res = E::update_many() // 构造批量更新语句
                    .filter(pk_col.eq(v.clone())) // 限定主键相等
                    .filter(del_col.is_null()) // 限定行未软删
                    .set(am) // 设置待更新字段
                    .exec(db) // 执行更新
                    .await?; // 等待并传播错误
                if res.rows_affected == 0 { // 没有命中任何行
                    return Err(DbErr::RecordNotFound( // 返回记录未找到
                        "row not found or logically deleted".to_string(), // 行不存在或已软删
                    ));
                }
                // 条件 update 不回传行：按主键回查（get 语义，已过滤软删行）
                return Self::select() // 回查更新后的行
                    .filter(pk_col.eq(v)) // 按主键过滤
                    .one(db) // 取一行
                    .await? // 等待结果
                    .ok_or_else(|| { // 无结果时构造错误
                        DbErr::RecordNotFound("row not found or logically deleted".to_string()) // 记录未找到错误
                    });
            }
        }
        if Self::deleted_at_column().is_some() { // 有软删列时走"先查后改"路径
            Self::ensure_alive(db, &am).await?; // 校验目标行未被软删
        }
        am.update(db).await // 执行原生更新并返回模型
    }

    /// 单列主键且主键值已提供时，校验行未被软删；其余情况跳过校验，
    /// 交由 sea-orm 原生行为（行不存在时报 RecordNotFound / RecordNotUpdated）。
    async fn ensure_alive<C: ConnectionTrait>(db: &C, am: &E::ActiveModel) -> Result<(), DbErr> { // 校验目标行存活（未软删）
        let Some(sea_orm::sea_query::ValueTuple::One(v)) = am.get_primary_key_value() else { // 非单列主键则直接放行
            return Ok(()); // 跳过校验
        };
        let Some(pk_col) = <E::PrimaryKey as Iterable>::iter() // 取主键列
            .next() // 首个主键列
            .map(|pk| pk.into_column()) // 转为列枚举
        else { // 无主键列时
            return Ok(()); // 跳过校验
        };
        if Self::select().filter(pk_col.eq(v)).one(db).await?.is_none() { // 按主键查存活行，查不到则
            return Err(DbErr::RecordNotFound( // 返回记录未找到
                "row not found or logically deleted".to_string(), // 行不存在或已软删
            ));
        }
        Ok(()) // 校验通过
    }

    /// 删除。有 deleted_at 列时执行软删（同时填充 updated_at），否则物理删除。
    /// 返回是否实际影响到行。
    pub async fn delete<C: ConnectionTrait>(&self, db: &C, id: PkOf<E>) -> Result<bool, DbErr> { // 删除一行（有软删列则软删），返回是否命中
        let (pk_col, pk_value) = Self::single_pk_value(id)?; // 解析主键为(列,值)

        match Self::deleted_at_column() { // 按是否存在软删列分支
            Some(col) => { // 有软删列：执行软删
                // deleted_at 须为时间类型列（Timestamp / TimestampWithTimeZone）
                let deleted_value = Self::now_value(col).ok_or_else(|| { // 生成软删时间值
                    DbErr::Custom("deleted_at column must be a timestamp type".to_string()) // 类型不符则报错
                })?;
                let mut upd = E::update_many() // 构造批量更新
                    .col_expr(col, sea_orm::sea_query::Expr::value(deleted_value)); // 将 deleted_at 置为当前时间
                if let Some(c) = Self::updated_at_column() { // 存在 updated_at 列时
                    if let Some(v) = Self::now_value(c) { // 且列类型支持时间时
                        upd = upd.col_expr(c, sea_orm::sea_query::Expr::value(v)); // 同时刷新 updated_at
                    }
                }
                // 只作用于存活行：对已删行重复 delete 返回 false（语义等同 get 返回 None）
                let res = upd // 在更新语句上继续追加条件
                    .filter(pk_col.eq(pk_value)) // 主键相等
                    .filter(col.is_null()) // 尚未软删
                    .exec(db) // 执行更新
                    .await?; // 等待并传播错误
                Ok(res.rows_affected > 0) // 命中行即返回 true
            }
            None => { // 无软删列：物理删除
                let res = E::delete_many().filter(pk_col.eq(pk_value)).exec(db).await?; // 按主键物理删除
                Ok(res.rows_affected > 0) // 命中行即返回 true
            }
        }
    }

    // ---------- 读 ----------

    /// 按主键查询（自动过滤已删行）
    pub async fn get<C: ConnectionTrait>( // 按主键查询单行
        &self, // 不可变借用 CRUD 服务
        db: &C, // 任意连接
        id: PkOf<E>, // 主键值
    ) -> Result<Option<E::Model>, DbErr> { // 返回可选模型或错误
        let mut sel = E::find_by_id(id); // 从按主键查询起步
        if let Some(cond) = Self::not_deleted_condition() { // 有软删列时
            sel = sel.filter(cond); // 附加"未删除"过滤
        }
        sel.one(db).await // 执行并取一行
    }

    /// 全量列表（自动过滤已删行）
    pub async fn list<C: ConnectionTrait>(&self, db: &C) -> Result<Vec<E::Model>, DbErr> { // 查询全部存活行
        Self::select().all(db).await // 执行基础查询取全部记录
    }

    /// 计数（自动过滤已删行）
    pub async fn count<C: ConnectionTrait>(&self, db: &C) -> Result<u64, DbErr> { // 统计存活行数
        Self::select().count(db).await // 对基础查询计数
    }

    /// 分页查询（自动过滤已删行；参数规范化见 [`PageParams`]）
    pub async fn page<C: ConnectionTrait>( // 分页查询存活行
        &self, // 不可变借用 CRUD 服务
        db: &C, // 任意连接
        q: &PageParams, // 分页参数
    ) -> Result<Paginated<E::Model>, DbErr> { // 返回分页结果或错误
        fetch_paginated(Self::select(), db, q).await // 对基础查询执行规范化分页
    }
}
