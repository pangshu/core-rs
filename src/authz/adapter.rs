//! Casbin 策略存储适配（文档 三·14）：文件（开发）或 DB（生产，经 SeaORM 读
//! `casbin_rule` 表）。[`DbOrFileAdapter`] 按 `[authz].source` 装配，
//! 两者对 Casbin 暴露同一 [`Adapter`] 契约，支持策略热更新（reload）。
//!
//! **表名前缀**：db 来源的物理表名 = `[authz].table_prefix` + `casbin_rule`，
//! 由配置在运行时决定（同一套代码与连接池，不同业务配不同前缀）。因此 DB 读写
//! 全部经 sea-query **运行时拼表名**，不依赖 `#[sea_orm(table_name = ...)]`
//! 生成的 `Entity::TABLE`（那是编译期常量，无法按配置变化）。

use sea_orm::entity::prelude::*; // 引入 SeaORM 实体派生宏与列标识（Column）等
use sea_orm::sea_query::{Expr, ExprTrait, Order, Query, TableName, TableRef}; // 引入运行时 SQL 构建类型
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, TransactionTrait}; // 引入连接/事务/错误类型
use casbin::error::AdapterError; // 引入 Casbin 适配器错误类型，用于包装底层 IO 错误
use casbin::{Adapter, Error as CasbinError, FileAdapter, Model as CasbinModel, Result as CasbinResult}; // 引入 Casbin 适配器 trait、错误/结果别名、文件适配器与模型 trait

use crate::config::sections::AuthzSettings; // 引入 [authz] 配置段，用于选择策略来源、路径与表前缀

/// Casbin 策略表的**基名**（无前缀）：物理表名 = `[authz].table_prefix` + 该基名。
///
/// 适配器与（应用侧）建表迁移共用此常量，避免命名规则两处硬编码而漂移。
pub const CASBIN_TABLE_BASE: &str = "casbin_rule";

/// 按前缀计算 Casbin 策略表的物理表名（适配器与建表迁移共用同一命名规则）。
pub fn casbin_table_name(prefix: &str) -> String { // 供应用侧迁移构造与适配器一致的表名
    format!("{prefix}{CASBIN_TABLE_BASE}") // 前缀 + 基名
}

/// `casbin_rule` 表实体：仅用于列标识（`Column::*`）与行结构（`Model`）。
///
/// 物理表名由运行时前缀决定（见 [`DbAdapter::table`]），故本模块的 DB 读写**不走**
/// `Entity::find()` 等（它们固定使用编译期常量 `Entity::TABLE`），而是用 sea-query
/// 按前缀动态拼名。
///
/// 注意：`#[sea_orm(table_name = ...)]` 是 `DeriveEntityModel` 的**必需属性**（缺了
/// 宏不生成 `Entity`，直接编译失败），其值只能是字面量（无法引用 [`CASBIN_TABLE_BASE`]），
/// 因此这里保持与基名一致的字面量；运行时表名请一律走 [`casbin_table_name`]。
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)] // 派生克隆/比较并生成 SeaORM 实体模型
#[sea_orm(table_name = "casbin_rule")] // 必需属性：值须等于 CASBIN_TABLE_BASE（宏只接受字面量）
pub struct Model { // 策略行实体：ptype 加 6 个通用值列
    #[sea_orm(primary_key, auto_increment = true)] // 主键列，自增
    pub id: i64, // 自增主键
    pub ptype: String, // 策略类型（p=策略，g=角色继承）
    pub v0: Option<String>, // 通用值列 0（可为空，按序填充）
    pub v1: Option<String>, // 通用值列 1
    pub v2: Option<String>, // 通用值列 2
    pub v3: Option<String>, // 通用值列 3
    pub v4: Option<String>, // 通用值列 4
    pub v5: Option<String>, // 通用值列 5
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] // 派生实体关系枚举所需 trait（此处无关系）
pub enum Relation {} // 空关系枚举：casbin_rule 表无外键关联

impl ActiveModelBehavior for ActiveModel {} // 使用默认的 ActiveModel 行为（无自定义钩子）

fn casbin_err(e: impl std::fmt::Display) -> CasbinError { // 把任意可显示错误统一包装成 Casbin 适配器错误
    CasbinError::AdapterError(AdapterError(Box::new(std::io::Error::other(e.to_string())))) // 借用 io::Error 承载错误文本后包进 AdapterError
}

type V = Option<String>; // 类型别名：单个策略值列（可为空）

fn rule_values(rule: &[String]) -> (V, V, V, V, V, V) { // 把一条策略规则铺开成 6 个值列（空串视为 None）
    let get = |i: usize| rule.get(i).filter(|s| !s.is_empty()).cloned(); // 取第 i 个值，空串或越界时返回 None
    (get(0), get(1), get(2), get(3), get(4), get(5)) // 依次取 6 个值组成元组返回
}

fn row_to_rule(ptype: &str, v: &[V]) -> Option<(String, Vec<String>)> { // 把数据库行还原成 Casbin 策略规则
    let mut rule = Vec::new(); // 收集非空的策略值
    for item in v { // 依次遍历 6 个值列
        match item { // 匹配当前值列
            Some(s) if !s.is_empty() => rule.push(s.clone()), // 非空值则克隆后追加到规则中
            // 首个空位之后的值视为不存在（casbin_rule 按序填充）
            _ => break, // 遇到空位即停止（后续列按约定必为空）
        }
    }
    if rule.is_empty() { // 若没有任何有效值
        None // 视为无效行，返回 None
    } else { // 存在有效值
        Some((ptype.to_string(), rule)) // 返回 (策略类型, 值列表)
    }
}

/// 把一条策略规则铺开成插入用的列表达式（ptype + 6 个值列，顺序与表列一致）
fn row_values(ptype: &str, rule: &[String]) -> Vec<Expr> { // 生成 INSERT 一行所需的表达式列表
    let (v0, v1, v2, v3, v4, v5) = rule_values(rule); // 先把规则铺开成 6 个值列
    vec![ // 按 (ptype, v0..v5) 顺序构造 7 个值表达式
        Expr::val(ptype.to_string()), // 策略类型
        Expr::val(v0), // 值列 0（Option → NULL）
        Expr::val(v1), // 值列 1
        Expr::val(v2), // 值列 2
        Expr::val(v3), // 值列 3
        Expr::val(v4), // 值列 4
        Expr::val(v5), // 值列 5
    ]
}

/// 由运行时表名构造 sea-query 的 `TableRef`（标识符会被后端自动加引号）
fn table_ref(table: &str) -> TableRef { // 把字符串表名转成可用的表引用
    TableRef::Table(TableName(None, table.to_string().into()), None) // 无 schema，仅表名（运行时前缀已拼好）
}

/// 可空列等值条件：Some 走 `= ?`，None 走 `IS NULL`（与 SeaORM `ColumnTrait::eq` 语义一致）
fn nullable_eq(col: Column, v: Option<String>) -> Expr { // 针对可空值列生成匹配表达式
    match v { // 按值是否为空分派
        Some(s) => Expr::col(col).eq(Expr::val(s)), // 有值：等值匹配
        None => Expr::col(col).is_null(), // 空值：IS NULL 匹配（不能用 = NULL）
    }
}

/// ptype 首字母 → 策略段名（p / g）
fn sec_of(ptype: &str) -> &str { // 由 ptype 推断 Casbin 策略段名
    match ptype.chars().next() { // 取 ptype 首字符判断
        Some('g') => "g", // 以 g 开头归入 g 段（角色继承）
        _ => "p", // 其余归入 p 段（权限策略）
    }
}

/// 策略来源：file（开发）/ db（生产），同一 Adapter 契约
pub enum DbOrFileAdapter { // 对外统一的策略适配器：内部委托给文件或数据库实现
    File(FileAdapter<String>), // 文件来源：Casbin 内置文件适配器
    Db(Box<DbAdapter>), // 数据库来源：自实现的 casbin_rule 表适配器（装箱以减小枚举体积）
}

impl DbOrFileAdapter { // 适配器的装配与注入逻辑
    pub async fn from_settings(settings: &AuthzSettings) -> Result<Self, crate::error::AppError> { // 按配置的 authz.source 创建适配器
        match settings.source.as_str() { // 依据来源字符串分支
            "file" => { // 文件来源分支
                if settings.file_path.is_empty() { // 文件路径为空说明配置不完整
                    return Err(crate::error::AppError::internal( // 返回内部配置错误
                        "authz.source = file but authz.file_path is empty", // 错误信息：选了 file 却没配 file_path
                    ));
                }
                Ok(Self::File(FileAdapter::new(settings.file_path.clone()))) // 用配置路径构造文件适配器
            }
            "db" => Ok(Self::Db(Box::new(DbAdapter { // 数据库来源：按配置前缀拼出物理表名
                db: None, // 连接暂缺，待 set_db 注入
                table: casbin_table_name(&settings.table_prefix), // 运行时表名 = 前缀 + 基名（与迁移共用规则）
            }))),
            other => Err(crate::error::AppError::internal(format!( // 未知来源：报配置错误
                "unknown authz.source: {other} (expected file / db)" // 错误信息：提示只支持 file / db
            ))),
        }
    }

    /// db 来源需在装配后注入连接池（App 装配时调用；配置加载早于连接池建立）
    pub fn set_db(&mut self, db: DatabaseConnection) { // 为 db 来源注入已建立的连接池
        if let Self::Db(a) = self { // 仅当当前是数据库来源时
            a.db = Some(db); // 把连接池写入内部适配器
        }
    }
}

#[async_trait::async_trait] // 为 trait 实现生成异步方法支持（因 trait 含 async fn）
impl Adapter for DbOrFileAdapter { // 实现 Casbin 的 Adapter 契约
    async fn load_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> { // 加载全部策略到模型
        match self { // 按内部来源分派
            Self::File(a) => a.load_policy(m).await, // 文件来源：委托文件适配器加载
            Self::Db(a) => a.load_policy(m).await, // 数据库来源：委托 DB 适配器加载
        }
    }

    async fn load_filtered_policy<'a>( // 按过滤器加载策略到模型
        &mut self, // 适配器自身
        m: &mut dyn CasbinModel, // 目标 Casbin 模型
        f: casbin::Filter<'a>, // 过滤条件
    ) -> CasbinResult<()> { // 返回加载结果
        match self { // 按内部来源分派
            Self::File(a) => a.load_filtered_policy(m, f).await, // 文件来源：委托文件适配器按过滤加载
            Self::Db(a) => a.load_policy(m).await, // 简化：全量加载
        }
    }

    async fn save_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> { // 把模型中的全部策略写回存储
        match self { // 按内部来源分派
            Self::File(a) => a.save_policy(m).await, // 文件来源：委托文件适配器保存
            Self::Db(a) => a.save_policy(m).await, // 数据库来源：委托 DB 适配器保存
        }
    }

    async fn clear_policy(&mut self) -> CasbinResult<()> { // 清空存储中的全部策略
        match self { // 按内部来源分派
            Self::File(a) => a.clear_policy().await, // 文件来源：委托文件适配器清空
            Self::Db(a) => a.clear_policy().await, // 数据库来源：委托 DB 适配器清空
        }
    }

    fn is_filtered(&self) -> bool { // 是否处于「已过滤加载」状态
        false // 本适配器不维护过滤态，恒为 false
    }

    async fn add_policy(&mut self, sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> { // 新增单条策略
        match self { // 按内部来源分派
            Self::File(a) => a.add_policy(sec, ptype, rule).await, // 文件来源：委托文件适配器新增
            Self::Db(a) => a.add_policy(sec, ptype, rule).await, // 数据库来源：委托 DB 适配器新增
        }
    }

    async fn add_policies( // 批量新增策略
        &mut self, // 适配器自身
        sec: &str, // 策略段名
        ptype: &str, // 策略类型
        rules: Vec<Vec<String>>, // 待新增的规则列表
    ) -> CasbinResult<bool> { // 返回是否新增成功
        match self { // 按内部来源分派
            Self::File(a) => a.add_policies(sec, ptype, rules).await, // 文件来源：委托文件适配器批量新增
            Self::Db(a) => a.add_policies(sec, ptype, rules).await, // 数据库来源：委托 DB 适配器批量新增
        }
    }

    async fn remove_policy(&mut self, sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> { // 移除单条策略
        match self { // 按内部来源分派
            Self::File(a) => a.remove_policy(sec, ptype, rule).await, // 文件来源：委托文件适配器移除
            Self::Db(a) => a.remove_policy(sec, ptype, rule).await, // 数据库来源：委托 DB 适配器移除
        }
    }

    async fn remove_policies( // 批量移除策略
        &mut self, // 适配器自身
        sec: &str, // 策略段名
        ptype: &str, // 策略类型
        rules: Vec<Vec<String>>, // 待移除的规则列表
    ) -> CasbinResult<bool> { // 返回是否移除成功
        match self { // 按内部来源分派
            Self::File(a) => a.remove_policies(sec, ptype, rules).await, // 文件来源：委托文件适配器批量移除
            Self::Db(a) => a.remove_policies(sec, ptype, rules).await, // 数据库来源：委托 DB 适配器批量移除
        }
    }

    async fn remove_filtered_policy( // 按字段过滤移除策略
        &mut self, // 适配器自身
        sec: &str, // 策略段名
        ptype: &str, // 策略类型
        field_index: usize, // 起始字段下标
        field_values: Vec<String>, // 字段匹配值
    ) -> CasbinResult<bool> { // 返回是否移除成功
        match self { // 按内部来源分派
            Self::File(a) => { // 文件来源分支
                a.remove_filtered_policy(sec, ptype, field_index, field_values) // 委托文件适配器按过滤移除
                    .await // 等待移除完成
            }
            Self::Db(a) => { // 数据库来源分支
                a.remove_filtered_policy(sec, ptype, field_index, field_values) // 委托 DB 适配器按过滤移除
                    .await // 等待移除完成
            }
        }
    }
}

/// DB 策略存储（生产）：按运行时表名对 `{prefix}casbin_rule` 表做 CRUD。
/// 连接由 [`DbOrFileAdapter::set_db`] 注入（配置加载早于连接池建立）。
pub struct DbAdapter { // 基于 SeaORM 的数据库策略适配器
    pub db: Option<DatabaseConnection>, // 数据库连接，装配后由 set_db 注入，可能暂为 None
    pub table: String, // 运行时物理表名（= [authz].table_prefix + casbin_rule）
}

fn db_err(e: DbErr) -> CasbinError { // 把 SeaORM 错误转换为 Casbin 错误
    casbin_err(e) // 复用通用转换（DbErr 实现了 Display）
}

fn conn_or_err(db: &Option<DatabaseConnection>) -> CasbinResult<&DatabaseConnection> { // 取出连接，缺失则报错
    db.as_ref() // 把 Option 转为可选引用
        .ok_or_else(|| casbin_err("casbin db adapter: database not configured yet")) // 未注入连接时返回明确错误
}

#[async_trait::async_trait] // 为 trait 实现生成异步方法支持
impl Adapter for DbAdapter { // 实现 Casbin 的 Adapter 契约（数据库版）
    async fn load_filtered_policy<'a>( // 按过滤器加载策略（此处忽略过滤器）
        &mut self, // 适配器自身
        m: &mut dyn CasbinModel, // 目标 Casbin 模型
        _f: casbin::Filter<'a>, // 过滤条件（未使用）
    ) -> CasbinResult<()> { // 返回加载结果
        // 简化：全量加载（过滤策略按需实现）
        self.load_policy(m).await // 直接走全量加载逻辑
    }

    fn is_filtered(&self) -> bool { // 是否处于「已过滤加载」状态
        false // 数据库适配器不支持过滤，恒为 false
    }

    async fn load_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> { // 从 {prefix}casbin_rule 表全量加载策略
        let conn = conn_or_err(&self.db)?; // 取出数据库连接（未注入则报错）
        m.clear_policy(); // 先清空模型中的旧策略，避免与库中数据叠加
        let mut q = Query::select(); // 构造 SELECT：按运行时表名取全部策略行
        q.column(Column::Id) // 选主键列
            .column(Column::Ptype) // 选策略类型列
            .column(Column::V0) // 选值列 0
            .column(Column::V1) // 选值列 1
            .column(Column::V2) // 选值列 2
            .column(Column::V3) // 选值列 3
            .column(Column::V4) // 选值列 4
            .column(Column::V5) // 选值列 5
            .from(table_ref(&self.table)) // 来源为运行时前缀表
            .order_by(Column::Id, Order::Asc); // 按主键升序，保证值列按写入顺序还原
        let rows = conn.query_all(&q).await.map_err(db_err)?; // 执行查询取出所有行
        for r in &rows { // 遍历每一行策略
            let ptype: String = r.try_get_by_index(1).map_err(db_err)?; // 第 2 列（下标 1）为策略类型
            let v: [V; 6] = [ // 第 3..8 列（下标 2..8）为 6 个值列
                r.try_get_by_index(2).map_err(db_err)?, // 值列 0
                r.try_get_by_index(3).map_err(db_err)?, // 值列 1
                r.try_get_by_index(4).map_err(db_err)?, // 值列 2
                r.try_get_by_index(5).map_err(db_err)?, // 值列 3
                r.try_get_by_index(6).map_err(db_err)?, // 值列 4
                r.try_get_by_index(7).map_err(db_err)?, // 值列 5
            ];
            if let Some((ptype, rule)) = row_to_rule(&ptype, &v) { // 把行还原成 (策略类型, 规则)
                m.add_policy(sec_of(&ptype), &ptype, rule); // 把策略加入模型对应段
            }
        }
        Ok(()) // 加载完成
    }

    async fn save_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> { // 把模型中的策略全量写回数据库
        let conn = conn_or_err(&self.db)?; // 取出数据库连接

        // 先在事务外读出全部规则，再在事务内 清空+写回：{prefix}casbin_rule 是授权
        // 唯一真相源，中途失败绝不能把表清成空（否则 reload 后全站 403）
        let mut all: Vec<Vec<Expr>> = Vec::new(); // 收集待写回的全部策略行（每行 7 个列表达式）
        let model = m.get_model(); // 取出模型内部结构以遍历策略
        for sec in ["p", "g"] { // 遍历策略段 p（权限）与 g（角色继承）
            if let Some(map) = model.get(sec) { // 若该段存在
                for (ptype, ast) in map { // 遍历该段下每种策略类型及其断言
                    for rule in ast.policy.iter() { // 遍历该类型下的每条规则
                        all.push(row_values(ptype, rule)); // 转成一行列表达式收集起来
                    }
                }
            }
        }

        let table = self.table.clone(); // 克隆表名以移入事务闭包
        conn.transaction(move |txn| { // 开启事务，保证清空与写回的原子性
            Box::pin(async move { // 把异步块装箱为事务回调要求的 Future
                let mut del = Query::delete(); // 构造删除语句
                del.from_table(table_ref(&table)); // 目标为运行时前缀表
                txn.execute(&del).await?; // 事务内先清空整张表
                if !all.is_empty() { // 若有规则待写入
                    let mut ins = Query::insert(); // 构造批量插入语句
                    ins.into_table(table_ref(&table)) // 目标为运行时前缀表
                        .columns([ // 固定列顺序：ptype + 6 个值列
                            Column::Ptype, // 策略类型
                            Column::V0, // 值列 0
                            Column::V1, // 值列 1
                            Column::V2, // 值列 2
                            Column::V3, // 值列 3
                            Column::V4, // 值列 4
                            Column::V5, // 值列 5
                        ])
                        .values_from_panic(all); // 批量写入全部策略行
                    txn.execute(&ins).await?; // 事务内执行批量插入
                }
                Ok::<(), DbErr>(()) // 显式标注成功类型，便于错误转换
            })
        })
        .await // 等待事务提交完成
        .map_err(casbin_err)?; // 事务失败转 Casbin 错误
        Ok(()) // 保存完成
    }

    async fn clear_policy(&mut self) -> CasbinResult<()> { // 清空 {prefix}casbin_rule 表
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let mut q = Query::delete(); // 构造删除语句
        q.from_table(table_ref(&self.table)); // 目标为运行时前缀表
        conn.execute(&q).await.map_err(db_err)?; // 删除表中全部行
        Ok(()) // 清空完成
    }

    async fn add_policy(&mut self, _sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> { // 向表中新增单条策略
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let mut q = Query::insert(); // 构造插入语句
        q.into_table(table_ref(&self.table)) // 目标为运行时前缀表
            .columns([ // 固定列顺序：ptype + 6 个值列
                Column::Ptype, // 策略类型
                Column::V0, // 值列 0
                Column::V1, // 值列 1
                Column::V2, // 值列 2
                Column::V3, // 值列 3
                Column::V4, // 值列 4
                Column::V5, // 值列 5
            ])
            .values_panic(row_values(ptype, &rule)); // 写入该规则的一行
        conn.execute(&q).await.map_err(db_err)?; // 在连接上执行插入
        Ok(true) // 插入成功，返回 true
    }

    async fn add_policies( // 向表中批量新增策略
        &mut self, // 适配器自身
        _sec: &str, // 策略段名（未使用，ptype 已足够）
        ptype: &str, // 策略类型
        rules: Vec<Vec<String>>, // 待新增的规则列表
    ) -> CasbinResult<bool> { // 返回是否成功
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let rows: Vec<Vec<Expr>> = rules.iter().map(|r| row_values(ptype, r)).collect(); // 把每条规则转成一行列表达式
        if !rows.is_empty() { // 若存在待插入行
            let mut q = Query::insert(); // 构造批量插入语句
            q.into_table(table_ref(&self.table)) // 目标为运行时前缀表
                .columns([ // 固定列顺序：ptype + 6 个值列
                    Column::Ptype, // 策略类型
                    Column::V0, // 值列 0
                    Column::V1, // 值列 1
                    Column::V2, // 值列 2
                    Column::V3, // 值列 3
                    Column::V4, // 值列 4
                    Column::V5, // 值列 5
                ])
                .values_from_panic(rows); // 批量写入全部策略行
            conn.execute(&q).await.map_err(db_err)?; // 执行批量插入
        }
        Ok(true) // 返回成功
    }

    async fn remove_policy(&mut self, _sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> { // 按各列精确匹配删除单条策略
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let (v0, v1, v2, v3, v4, v5) = rule_values(&rule); // 把规则铺开成 6 个值列用于匹配
        let mut q = Query::delete(); // 构造删除语句
        q.from_table(table_ref(&self.table)) // 目标为运行时前缀表
            .and_where(Expr::col(Column::Ptype).eq(Expr::val(ptype.to_string()))) // 条件：策略类型匹配
            .and_where(nullable_eq(Column::V0, v0)) // 条件：值列 0 匹配（空值走 IS NULL）
            .and_where(nullable_eq(Column::V1, v1)) // 条件：值列 1 匹配
            .and_where(nullable_eq(Column::V2, v2)) // 条件：值列 2 匹配
            .and_where(nullable_eq(Column::V3, v3)) // 条件：值列 3 匹配
            .and_where(nullable_eq(Column::V4, v4)) // 条件：值列 4 匹配
            .and_where(nullable_eq(Column::V5, v5)); // 条件：值列 5 匹配
        let res = conn.execute(&q).await.map_err(db_err)?; // 在连接上执行删除
        Ok(res.rows_affected() > 0) // 有行被删除则返回 true
    }

    async fn remove_policies( // 批量删除策略
        &mut self, // 适配器自身
        sec: &str, // 策略段名（透传给单条删除）
        ptype: &str, // 策略类型
        rules: Vec<Vec<String>>, // 待删除的规则列表
    ) -> CasbinResult<bool> { // 返回是否有任意删除成功
        let mut removed = false; // 累计是否删除了任意一行
        for rule in rules { // 逐条删除
            removed |= self.remove_policy(sec, ptype, rule).await?; // 复用单条删除并累计结果
        }
        Ok(removed) // 返回是否至少删除成功一条
    }

    async fn remove_filtered_policy( // 按字段下标与值构造条件删除策略
        &mut self, // 适配器自身
        _sec: &str, // 策略段名（未使用）
        ptype: &str, // 策略类型
        field_index: usize, // 起始字段下标（映射到 v 列）
        field_values: Vec<String>, // 各字段的匹配值
    ) -> CasbinResult<bool> { // 返回是否有行被删除
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let mut q = Query::delete(); // 构造删除语句
        q.from_table(table_ref(&self.table)); // 目标为运行时前缀表
        q.and_where(Expr::col(Column::Ptype).eq(Expr::val(ptype.to_string()))); // 基础条件：策略类型匹配
        for (i, value) in field_values.iter().enumerate() { // 遍历每个字段值，下标 i 为相对偏移
            if value.is_empty() { // 空值表示该字段不参与过滤
                continue; // 跳过空值
            }
            let col = match field_index + i { // 把绝对字段下标映射到具体值列
                0 => Column::V0, // 下标 0 对应 v0
                1 => Column::V1, // 下标 1 对应 v1
                2 => Column::V2, // 下标 2 对应 v2
                3 => Column::V3, // 下标 3 对应 v3
                4 => Column::V4, // 下标 4 对应 v4
                5 => Column::V5, // 下标 5 对应 v5
                _ => break, // 超出列范围则停止（无更多可匹配列）
            };
            q.and_where(Expr::col(col).eq(Expr::val(value.clone()))); // 追加该列的等值条件
        }
        let res = conn.execute(&q).await.map_err(db_err)?; // 在连接上执行删除
        Ok(res.rows_affected() > 0) // 有行被删除则返回 true
    }
}
