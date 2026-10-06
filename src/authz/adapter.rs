//! Casbin 策略存储适配（文档 三·14）：文件（开发）或 DB（生产，经 SeaORM 读
//! `casbin_rule` 表）。[`DbOrFileAdapter`] 按 `[authz].source` 装配，
//! 两者对 Casbin 暴露同一 [`Adapter`] 契约，支持策略热更新（reload）。

use sea_orm::entity::prelude::*;
use casbin::error::AdapterError;
use casbin::{Adapter, Error as CasbinError, FileAdapter, Model as CasbinModel, Result as CasbinResult};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, QueryOrder, Set, TransactionTrait};

use crate::config::sections::AuthzSettings;

/// `casbin_rule` 表实体（通用策略表，无业务词汇；建表迁移由应用侧提供）
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "casbin_rule")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    pub ptype: String,
    pub v0: Option<String>,
    pub v1: Option<String>,
    pub v2: Option<String>,
    pub v3: Option<String>,
    pub v4: Option<String>,
    pub v5: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

fn casbin_err(e: impl std::fmt::Display) -> CasbinError {
    CasbinError::AdapterError(AdapterError(Box::new(std::io::Error::other(e.to_string()))))
}

type V = Option<String>;

fn rule_values(rule: &[String]) -> (V, V, V, V, V, V) {
    let get = |i: usize| rule.get(i).filter(|s| !s.is_empty()).cloned();
    (get(0), get(1), get(2), get(3), get(4), get(5))
}

fn row_to_rule(ptype: &str, v: &[V]) -> Option<(String, Vec<String>)> {
    let mut rule = Vec::new();
    for item in v {
        match item {
            Some(s) if !s.is_empty() => rule.push(s.clone()),
            // 首个空位之后的值视为不存在（casbin_rule 按序填充）
            _ => break,
        }
    }
    if rule.is_empty() {
        None
    } else {
        Some((ptype.to_string(), rule))
    }
}

fn active_row(ptype: &str, rule: &[String]) -> ActiveModel {
    let (v0, v1, v2, v3, v4, v5) = rule_values(rule);
    ActiveModel {
        id: Default::default(),
        ptype: Set(ptype.to_string()),
        v0: Set(v0),
        v1: Set(v1),
        v2: Set(v2),
        v3: Set(v3),
        v4: Set(v4),
        v5: Set(v5),
    }
}

/// ptype 首字母 → 策略段名（p / g）
fn sec_of(ptype: &str) -> &str {
    match ptype.chars().next() {
        Some('g') => "g",
        _ => "p",
    }
}

/// 策略来源：file（开发）/ db（生产），同一 Adapter 契约
pub enum DbOrFileAdapter {
    File(FileAdapter<String>),
    Db(Box<DbAdapter>),
}

impl DbOrFileAdapter {
    pub async fn from_settings(settings: &AuthzSettings) -> Result<Self, crate::error::AppError> {
        match settings.source.as_str() {
            "file" => {
                if settings.file_path.is_empty() {
                    return Err(crate::error::AppError::internal(
                        "authz.source = file but authz.file_path is empty",
                    ));
                }
                Ok(Self::File(FileAdapter::new(settings.file_path.clone())))
            }
            "db" => Ok(Self::Db(Box::new(DbAdapter { db: None }))),
            other => Err(crate::error::AppError::internal(format!(
                "unknown authz.source: {other} (expected file / db)"
            ))),
        }
    }

    /// db 来源需在装配后注入连接池（App 装配时调用；配置加载早于连接池建立）
    pub fn set_db(&mut self, db: DatabaseConnection) {
        if let Self::Db(a) = self {
            a.db = Some(db);
        }
    }
}

#[async_trait::async_trait]
impl Adapter for DbOrFileAdapter {
    async fn load_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> {
        match self {
            Self::File(a) => a.load_policy(m).await,
            Self::Db(a) => a.load_policy(m).await,
        }
    }

    async fn load_filtered_policy<'a>(
        &mut self,
        m: &mut dyn CasbinModel,
        f: casbin::Filter<'a>,
    ) -> CasbinResult<()> {
        match self {
            Self::File(a) => a.load_filtered_policy(m, f).await,
            Self::Db(a) => a.load_policy(m).await, // 简化：全量加载
        }
    }

    async fn save_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> {
        match self {
            Self::File(a) => a.save_policy(m).await,
            Self::Db(a) => a.save_policy(m).await,
        }
    }

    async fn clear_policy(&mut self) -> CasbinResult<()> {
        match self {
            Self::File(a) => a.clear_policy().await,
            Self::Db(a) => a.clear_policy().await,
        }
    }

    fn is_filtered(&self) -> bool {
        false
    }

    async fn add_policy(&mut self, sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> {
        match self {
            Self::File(a) => a.add_policy(sec, ptype, rule).await,
            Self::Db(a) => a.add_policy(sec, ptype, rule).await,
        }
    }

    async fn add_policies(
        &mut self,
        sec: &str,
        ptype: &str,
        rules: Vec<Vec<String>>,
    ) -> CasbinResult<bool> {
        match self {
            Self::File(a) => a.add_policies(sec, ptype, rules).await,
            Self::Db(a) => a.add_policies(sec, ptype, rules).await,
        }
    }

    async fn remove_policy(&mut self, sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> {
        match self {
            Self::File(a) => a.remove_policy(sec, ptype, rule).await,
            Self::Db(a) => a.remove_policy(sec, ptype, rule).await,
        }
    }

    async fn remove_policies(
        &mut self,
        sec: &str,
        ptype: &str,
        rules: Vec<Vec<String>>,
    ) -> CasbinResult<bool> {
        match self {
            Self::File(a) => a.remove_policies(sec, ptype, rules).await,
            Self::Db(a) => a.remove_policies(sec, ptype, rules).await,
        }
    }

    async fn remove_filtered_policy(
        &mut self,
        sec: &str,
        ptype: &str,
        field_index: usize,
        field_values: Vec<String>,
    ) -> CasbinResult<bool> {
        match self {
            Self::File(a) => {
                a.remove_filtered_policy(sec, ptype, field_index, field_values)
                    .await
            }
            Self::Db(a) => {
                a.remove_filtered_policy(sec, ptype, field_index, field_values)
                    .await
            }
        }
    }
}

/// DB 策略存储（生产）：casbin_rule 表 CRUD。连接由 [`DbOrFileAdapter::set_db`]
/// 注入（配置加载早于连接池建立）。
pub struct DbAdapter {
    pub db: Option<DatabaseConnection>,
}

fn db_err(e: DbErr) -> CasbinError {
    casbin_err(e)
}

fn conn_or_err(db: &Option<DatabaseConnection>) -> CasbinResult<&DatabaseConnection> {
    db.as_ref()
        .ok_or_else(|| casbin_err("casbin db adapter: database not configured yet"))
}

#[async_trait::async_trait]
impl Adapter for DbAdapter {
    async fn load_filtered_policy<'a>(
        &mut self,
        m: &mut dyn CasbinModel,
        _f: casbin::Filter<'a>,
    ) -> CasbinResult<()> {
        // 简化：全量加载（过滤策略按需实现）
        self.load_policy(m).await
    }

    fn is_filtered(&self) -> bool {
        false
    }

    async fn load_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> {
        let conn = conn_or_err(&self.db)?;
        m.clear_policy();
        let rows = Entity::find()
            .order_by_asc(Column::Id)
            .all(conn)
            .await
            .map_err(db_err)?;
        for r in &rows {
            if let Some((ptype, rule)) = row_to_rule(
                &r.ptype,
                &[r.v0.clone(), r.v1.clone(), r.v2.clone(), r.v3.clone(), r.v4.clone(), r.v5.clone()],
            ) {
                m.add_policy(sec_of(&ptype), &ptype, rule);
            }
        }
        Ok(())
    }

    async fn save_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> {
        let conn = conn_or_err(&self.db)?;

        // 先在事务外读出全部规则，再在事务内 清空+写回：casbin_rule 是授权唯一
        // 真相源，中途失败绝不能把表清成空（否则 reload 后全站 403）
        let mut all = Vec::new();
        let model = m.get_model();
        for sec in ["p", "g"] {
            if let Some(map) = model.get(sec) {
                for (ptype, ast) in map {
                    for rule in ast.policy.iter() {
                        all.push(active_row(ptype, rule));
                    }
                }
            }
        }

        conn.transaction(move |txn| {
            Box::pin(async move {
                Entity::delete_many().exec(txn).await?;
                if !all.is_empty() {
                    Entity::insert_many(all).exec(txn).await?;
                }
                Ok::<(), DbErr>(())
            })
        })
        .await
        .map_err(casbin_err)?;
        Ok(())
    }

    async fn clear_policy(&mut self) -> CasbinResult<()> {
        let conn = conn_or_err(&self.db)?;
        Entity::delete_many().exec(conn).await.map_err(db_err)?;
        Ok(())
    }

    async fn add_policy(&mut self, _sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> {
        let conn = conn_or_err(&self.db)?;
        Entity::insert(active_row(ptype, &rule))
            .exec(conn)
            .await
            .map_err(db_err)?;
        Ok(true)
    }

    async fn add_policies(
        &mut self,
        _sec: &str,
        ptype: &str,
        rules: Vec<Vec<String>>,
    ) -> CasbinResult<bool> {
        let conn = conn_or_err(&self.db)?;
        let models: Vec<ActiveModel> = rules.iter().map(|r| active_row(ptype, r)).collect();
        if !models.is_empty() {
            Entity::insert_many(models).exec(conn).await.map_err(db_err)?;
        }
        Ok(true)
    }

    async fn remove_policy(&mut self, _sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> {
        let conn = conn_or_err(&self.db)?;
        let (v0, v1, v2, v3, v4, v5) = rule_values(&rule);
        let res = Entity::delete_many()
            .filter(Column::Ptype.eq(ptype))
            .filter(Column::V0.eq(v0))
            .filter(Column::V1.eq(v1))
            .filter(Column::V2.eq(v2))
            .filter(Column::V3.eq(v3))
            .filter(Column::V4.eq(v4))
            .filter(Column::V5.eq(v5))
            .exec(conn)
            .await
            .map_err(db_err)?;
        Ok(res.rows_affected > 0)
    }

    async fn remove_policies(
        &mut self,
        sec: &str,
        ptype: &str,
        rules: Vec<Vec<String>>,
    ) -> CasbinResult<bool> {
        let mut removed = false;
        for rule in rules {
            removed |= self.remove_policy(sec, ptype, rule).await?;
        }
        Ok(removed)
    }

    async fn remove_filtered_policy(
        &mut self,
        _sec: &str,
        ptype: &str,
        field_index: usize,
        field_values: Vec<String>,
    ) -> CasbinResult<bool> {
        let conn = conn_or_err(&self.db)?;
        let mut cond = Condition::all().add(Column::Ptype.eq(ptype));
        for (i, value) in field_values.iter().enumerate() {
            if value.is_empty() {
                continue;
            }
            let col = match field_index + i {
                0 => Column::V0,
                1 => Column::V1,
                2 => Column::V2,
                3 => Column::V3,
                4 => Column::V4,
                5 => Column::V5,
                _ => break,
            };
            cond = cond.add(col.eq(value.clone()));
        }
        let res = Entity::delete_many()
            .filter(cond)
            .exec(conn)
            .await
            .map_err(db_err)?;
        Ok(res.rows_affected > 0)
    }
}
