//! 通用搜索 DSL：查询字符串 → SeaORM 条件（对标 go-admin-core tools/search）。
//!
//! Go 版用 struct tag `search:"type:contains;column:name"` 声明；Rust 版遵循
//! 框架「约定优于配置、零注册」铁律，改用 **Django 风格查询参数**：
//!
//! ```text
//! ?name__contains=abc          → name LIKE '%abc%'
//! ?age__gte=18&status__in=1,2  → age >= 18 AND status IN (1, 2)
//! ?created_at__isnull=false    → created_at IS NOT NULL
//! ?sort=-created_at,name       → ORDER BY created_at DESC, name ASC
//! ```
//!
//! 安全模型：
//! - **列名白名单**：只接受实体真实存在的列（`E::Column::from_str` 精确匹配），
//!   未知列静默忽略（debug 日志），不存在 SQL 注入面；
//! - **操作符白名单**：未知操作符直接 400；
//! - **值类型化**：按列声明类型解析（int/bool/float/时间戳），解析失败 400，
//!   传给 SeaORM 的是绑定参数而非拼接字符串。
//!
//! 空值约定：`field=`（空串）视为未过滤；判空请用 `field__isnull=true|false`。

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use sea_orm::sea_query::SimpleExpr;
use sea_orm::sea_query::{Expr, ExprTrait, Func, Value};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, Select};

use crate::error::AppError;

/// 搜索条件提取器：`?name__contains=abc&age__gte=18&sort=-created_at`。
/// handler 签名写 `q: SearchQuery`，与 [`crate::web::extract::PageQuery`] 并存。
///
/// ```rust,no_run
/// # use core_rs::prelude::*;
/// # use core_rs::orm::search::SearchQuery;
/// # mod user {
/// #     use sea_orm::entity::prelude::*;
/// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// #     #[sea_orm(table_name = "user")]
/// #     pub struct Model {
/// #         #[sea_orm(primary_key)]
/// #         pub id: i32,
/// #         pub name: String,
/// #         pub age: i32,
/// #         pub created_at: DateTimeUtc,
/// #     }
/// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
/// #     pub enum Relation {}
/// #     impl ActiveModelBehavior for ActiveModel {}
/// # }
/// # async fn demo(db: Db, s: SearchQuery) -> Result<(), AppError> {
/// // GET /users?name__contains=li&age__gte=18&sort=-name
/// let q = PageQuery { page: 1, size: 10 };
/// let paginator = user::Entity::find()
///     .apply_search(&s)?          // 列白名单校验：未知列忽略、未知操作符 400
///     .paginate(&db.0, q.limit());
/// let total = paginator.num_items().await?;
/// let records = paginator.fetch_page(q.page_index()).await?;
/// # let _ = (total, records);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    filters: Vec<Filter>,
    sort: Vec<(String, bool)>,
}

#[derive(Debug, Clone)]
struct Filter {
    column: String,
    op: Op,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Exact,
    Ne,
    Contains,
    IContains,
    StartsWith,
    IStartsWith,
    EndsWith,
    IEndsWith,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    IsNull,
}

impl Op {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "exact" | "eq" | "" => Self::Exact,
            "ne" | "neq" => Self::Ne,
            "contains" => Self::Contains,
            "icontains" => Self::IContains,
            "startswith" => Self::StartsWith,
            "istartswith" => Self::IStartsWith,
            "endswith" => Self::EndsWith,
            "iendswith" => Self::IEndsWith,
            "gt" => Self::Gt,
            "gte" => Self::Gte,
            "lt" => Self::Lt,
            "lte" => Self::Lte,
            "in" => Self::In,
            "isnull" => Self::IsNull,
            _ => return None,
        })
    }
}

impl SearchQuery {
    /// 从原始查询串解析（axum 自动做 percent 解码后的 k=v 对）
    pub fn parse(pairs: impl IntoIterator<Item = (String, String)>) -> Result<Self, AppError> {
        let mut q = Self::default();
        let mut order_hint: Option<bool> = None;
        for (k, v) in pairs {
            match k.as_str() {
                "page" | "size" => continue,
                "sort" => {
                    q.sort = parse_sort(&v)?;
                    continue;
                }
                "order" => {
                    order_hint = Some(v.eq_ignore_ascii_case("desc"));
                    continue;
                }
                _ => {}
            }
            if v.is_empty() {
                continue; // field= 空值视为未过滤
            }
            let (column, op) = match k.rsplit_once("__") {
                Some((col, suffix)) => {
                    let Some(op) = Op::parse(suffix) else {
                        return Err(AppError::bad_request(format!(
                            "unknown search operator `{suffix}` in `{k}`"
                        )));
                    };
                    (col.to_string(), op)
                }
                None => (k, Op::Exact),
            };
            q.filters.push(Filter { column, op, value: v });
        }
        if let Some(desc) = order_hint {
            for (_, d) in &mut q.sort {
                // `order=desc` 对未显式带 `-` 前缀的排序列统一生效
                if !*d {
                    *d = desc;
                }
            }
        }
        Ok(q)
    }

    /// 将过滤与排序应用到任意 Select（列白名单校验，未知列忽略）。
    pub fn apply<E: EntityTrait>(&self, mut select: Select<E>) -> Result<Select<E>, AppError> {
        if self.filters.is_empty() && self.sort.is_empty() {
            return Ok(select);
        }
        let mut condition = Condition::all();
        let mut count = 0usize;
        for f in &self.filters {
            let Some(col) = column_of::<E>(&f.column) else {
                tracing::debug!(column = %f.column, "search: unknown column ignored");
                continue;
            };
            condition = condition.add(build_condition::<E>(col, f)?);
            count += 1;
        }
        if count > 0 {
            select = select.filter(condition);
        }
        for (name, desc) in &self.sort {
            let Some(col) = column_of::<E>(name) else {
                tracing::debug!(column = %name, "sort: unknown column ignored");
                continue;
            };
            select = select.order_by(col, if *desc { Order::Desc } else { Order::Asc });
        }
        Ok(select)
    }
}

/// [`Select`] 扩展：链式写法 `Entity::find().apply_search(&q)?`，
/// 等价于 `q.apply(Entity::find())?`。
pub trait SearchApply<E: EntityTrait> {
    fn apply_search(self, q: &SearchQuery) -> Result<Select<E>, AppError>;
}

impl<E: EntityTrait> SearchApply<E> for Select<E> {
    fn apply_search(self, q: &SearchQuery) -> Result<Select<E>, AppError> {
        q.apply(self)
    }
}

/// 解析 `sort=-created_at,name`
fn parse_sort(v: &str) -> Result<Vec<(String, bool)>, AppError> {
    let mut out = Vec::new();
    for part in v.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (desc, name) = match part.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, part.strip_prefix('+').unwrap_or(part)),
        };
        if name.is_empty() {
            return Err(AppError::bad_request(format!("invalid sort field `{part}`")));
        }
        out.push((name.to_string(), desc));
    }
    Ok(out)
}

/// 列名白名单：只接受实体真实存在的列
fn column_of<E: EntityTrait>(name: &str) -> Option<E::Column> {
    E::Column::from_str(name).ok()
}

/// 大小写不敏感模糊匹配：`LOWER(col) LIKE pattern`（各数据库通用的等价写法；
/// 列来自白名单枚举，无注入面）。MySQL 默认 ci 排序规则本身不区分大小写，
/// 该写法在 PG 上等价 ILIKE。
fn lower_like(expr: impl Into<SimpleExpr>, pattern: String) -> SimpleExpr {
    Func::lower(expr).like(pattern).into()
}

/// 按操作符构造条件；值按列类型解析，失败返回 400
fn build_condition<E: EntityTrait>(col: E::Column, f: &Filter) -> Result<SimpleExpr, AppError> {
    use Op::*;
    match f.op {
        Exact => Ok(col.eq(typed_value::<E>(col, &f.value)?)),
        Ne => Ok(col.ne(typed_value::<E>(col, &f.value)?)),
        Gt => Ok(col.gt(typed_value::<E>(col, &f.value)?)),
        Gte => Ok(col.gte(typed_value::<E>(col, &f.value)?)),
        Lt => Ok(col.lt(typed_value::<E>(col, &f.value)?)),
        Lte => Ok(col.lte(typed_value::<E>(col, &f.value)?)),
        Contains => Ok(col.contains(&f.value)),
        IContains => Ok(lower_like(Expr::col(col), format!("%{}%", f.value))),
        StartsWith => Ok(col.starts_with(&f.value)),
        IStartsWith => Ok(lower_like(Expr::col(col), format!("{}%", f.value))),
        EndsWith => Ok(col.ends_with(&f.value)),
        IEndsWith => Ok(lower_like(Expr::col(col), format!("%{}", f.value))),
        In => {
            let mut values = Vec::new();
            for raw in f.value.split(',') {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                values.push(typed_value::<E>(col, raw)?);
            }
            if values.is_empty() {
                return Err(AppError::bad_request(format!(
                    "empty `in` list for column `{}`",
                    f.column
                )));
            }
            Ok(col.is_in(values))
        }
        IsNull => Ok(match f.value.eq_ignore_ascii_case("true") {
            true => col.is_null(),
            false => col.is_not_null(),
        }),
    }
}

/// 按列声明类型把字符串解析为绑定值；解析失败返回 400（显式报错优于静默吞掉）
fn typed_value<E: EntityTrait>(col: E::Column, raw: &str) -> Result<Value, AppError> {
    use sea_orm::ColumnType;
    let bad = || {
        AppError::bad_request(format!(
            "invalid value `{raw}` for column type {:?}",
            col.def().get_column_type()
        ))
    };
    Ok(match col.def().get_column_type() {
        ColumnType::TinyInteger
        | ColumnType::SmallInteger
        | ColumnType::Integer
        | ColumnType::BigInteger => raw.parse::<i64>().map(Value::from).map_err(|_| bad())?,
        ColumnType::TinyUnsigned
        | ColumnType::SmallUnsigned
        | ColumnType::Unsigned
        | ColumnType::BigUnsigned => raw.parse::<u64>().map(Value::from).map_err(|_| bad())?,
        ColumnType::Float | ColumnType::Double | ColumnType::Decimal(_) | ColumnType::Money(_) => {
            raw.parse::<f64>().map(Value::from).map_err(|_| bad())?
        }
        ColumnType::Boolean => raw
            .parse::<bool>()
            .or_else(|_| match raw {
                "1" => Ok(true),
                "0" => Ok(false),
                _ => Err(()),
            })
            .map(Value::from)
            .map_err(|_| bad())?,
        ColumnType::Timestamp | ColumnType::DateTime => raw
            .parse::<chrono::NaiveDateTime>()
            .or_else(|_| {
                chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                    .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
            })
            .map_err(|_| bad())
            .map(Value::from)?,
        ColumnType::TimestampWithTimeZone => raw
            .parse::<chrono::DateTime<chrono::Utc>>()
            .map_err(|_| bad())
            .map(Value::from)?,
        _ => raw.to_string().into(),
    })
}

impl<S> FromRequestParts<S> for SearchQuery
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        use axum::extract::Query;
        // 只借用 URI 查询串做解析，不消费请求体
        let pairs = Query::<std::collections::HashMap<String, String>>::from_request_parts(
            parts, _state,
        )
        .await
        .map_err(|rej| AppError::bad_request(rej.body_text()))?;
        SearchQuery::parse(pairs.0.into_iter())
    }
}
