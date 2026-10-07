//! Casbin 策略存储适配（文档 三·14）：文件（开发）或 DB（生产，经 SeaORM 读
//! `casbin_rule` 表）。[`DbOrFileAdapter`] 按 `[authz].source` 装配，
//! 两者对 Casbin 暴露同一 [`Adapter`] 契约，支持策略热更新（reload）。

use sea_orm::entity::prelude::*; // 引入 SeaORM 实体派生所需宏与类型（DeriveEntityModel 等）
use casbin::error::AdapterError; // 引入 Casbin 适配器错误类型，用于包装底层 IO 错误
use casbin::{Adapter, Error as CasbinError, FileAdapter, Model as CasbinModel, Result as CasbinResult}; // 引入 Casbin 适配器 trait、错误/结果别名、文件适配器与模型 trait
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, QueryOrder, Set, TransactionTrait}; // 引入 SeaORM 查询/条件/连接/事务等类型

use crate::config::sections::AuthzSettings; // 引入 [authz] 配置段，用于选择策略来源与路径

/// `casbin_rule` 表实体（通用策略表，无业务词汇；建表迁移由应用侧提供）
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)] // 派生克隆/比较并生成 SeaORM 实体模型
#[sea_orm(table_name = "casbin_rule")] // 指定实体对应的数据库表名为 casbin_rule
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

fn active_row(ptype: &str, rule: &[String]) -> ActiveModel { // 把一条策略规则构造成可插入的 ActiveModel
    let (v0, v1, v2, v3, v4, v5) = rule_values(rule); // 先把规则铺开成 6 个值列
    ActiveModel { // 构造插入用的活动模型
        id: Default::default(), // 主键交给数据库自增
        ptype: Set(ptype.to_string()), // 设置策略类型
        v0: Set(v0), // 设置值列 0
        v1: Set(v1), // 设置值列 1
        v2: Set(v2), // 设置值列 2
        v3: Set(v3), // 设置值列 3
        v4: Set(v4), // 设置值列 4
        v5: Set(v5), // 设置值列 5
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
            "db" => Ok(Self::Db(Box::new(DbAdapter { db: None }))), // 数据库来源：先返回空连接适配器，待 set_db 注入
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

/// DB 策略存储（生产）：casbin_rule 表 CRUD。连接由 [`DbOrFileAdapter::set_db`]
/// 注入（配置加载早于连接池建立）。
pub struct DbAdapter { // 基于 SeaORM 的数据库策略适配器
    pub db: Option<DatabaseConnection>, // 数据库连接，装配后由 set_db 注入，可能暂为 None
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

    async fn load_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> { // 从 casbin_rule 表全量加载策略
        let conn = conn_or_err(&self.db)?; // 取出数据库连接（未注入则报错）
        m.clear_policy(); // 先清空模型中的旧策略，避免与库中数据叠加
        let rows = Entity::find() // 构造查询：选择全部策略行
            .order_by_asc(Column::Id) // 按主键升序，保证策略值列按写入顺序还原
            .all(conn) // 执行查询取出所有行
            .await // 等待查询完成
            .map_err(db_err)?; // 查询失败转 Casbin 错误
        for r in &rows { // 遍历每一行策略
            if let Some((ptype, rule)) = row_to_rule( // 把行还原成 (策略类型, 规则)
                &r.ptype, // 该行的策略类型
                &[r.v0.clone(), r.v1.clone(), r.v2.clone(), r.v3.clone(), r.v4.clone(), r.v5.clone()], // 6 个值列拼成数组
            ) { // 仅当行有效时
                m.add_policy(sec_of(&ptype), &ptype, rule); // 把策略加入模型对应段
            }
        }
        Ok(()) // 加载完成
    }

    async fn save_policy(&mut self, m: &mut dyn CasbinModel) -> CasbinResult<()> { // 把模型中的策略全量写回数据库
        let conn = conn_or_err(&self.db)?; // 取出数据库连接

        // 先在事务外读出全部规则，再在事务内 清空+写回：casbin_rule 是授权唯一
        // 真相源，中途失败绝不能把表清成空（否则 reload 后全站 403）
        let mut all = Vec::new(); // 收集待写回的全部策略行
        let model = m.get_model(); // 取出模型内部结构以遍历策略
        for sec in ["p", "g"] { // 遍历策略段 p（权限）与 g（角色继承）
            if let Some(map) = model.get(sec) { // 若该段存在
                for (ptype, ast) in map { // 遍历该段下每种策略类型及其断言
                    for rule in ast.policy.iter() { // 遍历该类型下的每条规则
                        all.push(active_row(ptype, rule)); // 转成 ActiveModel 收集起来
                    }
                }
            }
        }

        conn.transaction(move |txn| { // 开启事务，保证清空与写回的原子性
            Box::pin(async move { // 把异步块装箱为事务回调要求的 Future
                Entity::delete_many().exec(txn).await?; // 事务内先清空整张表
                if !all.is_empty() { // 若有规则待写入
                    Entity::insert_many(all).exec(txn).await?; // 事务内批量插入全部策略
                }
                Ok::<(), DbErr>(()) // 显式标注成功类型，便于错误转换
            })
        })
        .await // 等待事务提交完成
        .map_err(casbin_err)?; // 事务失败转 Casbin 错误
        Ok(()) // 保存完成
    }

    async fn clear_policy(&mut self) -> CasbinResult<()> { // 清空 casbin_rule 表
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        Entity::delete_many().exec(conn).await.map_err(db_err)?; // 删除表中全部行
        Ok(()) // 清空完成
    }

    async fn add_policy(&mut self, _sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> { // 向表中新增单条策略
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        Entity::insert(active_row(ptype, &rule)) // 把规则转成 ActiveModel 并构造插入
            .exec(conn) // 在连接上执行插入
            .await // 等待插入完成
            .map_err(db_err)?; // 插入失败转 Casbin 错误
        Ok(true) // 插入成功，返回 true
    }

    async fn add_policies( // 向表中批量新增策略
        &mut self, // 适配器自身
        _sec: &str, // 策略段名（未使用，ptype 已足够）
        ptype: &str, // 策略类型
        rules: Vec<Vec<String>>, // 待新增的规则列表
    ) -> CasbinResult<bool> { // 返回是否成功
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let models: Vec<ActiveModel> = rules.iter().map(|r| active_row(ptype, r)).collect(); // 把每条规则转成 ActiveModel
        if !models.is_empty() { // 若存在待插入行
            Entity::insert_many(models).exec(conn).await.map_err(db_err)?; // 批量插入全部策略行
        }
        Ok(true) // 返回成功
    }

    async fn remove_policy(&mut self, _sec: &str, ptype: &str, rule: Vec<String>) -> CasbinResult<bool> { // 按各列精确匹配删除单条策略
        let conn = conn_or_err(&self.db)?; // 取出数据库连接
        let (v0, v1, v2, v3, v4, v5) = rule_values(&rule); // 把规则铺开成 6 个值列用于匹配
        let res = Entity::delete_many() // 构造删除查询
            .filter(Column::Ptype.eq(ptype)) // 条件：策略类型匹配
            .filter(Column::V0.eq(v0)) // 条件：值列 0 匹配
            .filter(Column::V1.eq(v1)) // 条件：值列 1 匹配
            .filter(Column::V2.eq(v2)) // 条件：值列 2 匹配
            .filter(Column::V3.eq(v3)) // 条件：值列 3 匹配
            .filter(Column::V4.eq(v4)) // 条件：值列 4 匹配
            .filter(Column::V5.eq(v5)) // 条件：值列 5 匹配
            .exec(conn) // 在连接上执行删除
            .await // 等待删除完成
            .map_err(db_err)?; // 删除失败转 Casbin 错误
        Ok(res.rows_affected > 0) // 有行被删除则返回 true
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
        let mut cond = Condition::all().add(Column::Ptype.eq(ptype)); // 基础条件：策略类型匹配
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
            cond = cond.add(col.eq(value.clone())); // 追加该列的等值条件
        }
        let res = Entity::delete_many() // 构造删除查询
            .filter(cond) // 应用累积的过滤条件
            .exec(conn) // 在连接上执行删除
            .await // 等待删除完成
            .map_err(db_err)?; // 删除失败转 Casbin 错误
        Ok(res.rows_affected > 0) // 有行被删除则返回 true
    }
}
