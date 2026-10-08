//! 授权（能做什么，文档 三·14）：Casbin RBAC —— 模型与策略由 config 集中配置。
//!
//! - [`model`]：Casbin 模型（RBAC 基础版 / RBAC with domains 多租户版）；
//! - [`adapter`]：策略存储 —— 文件（开发）/ DB（生产，经 SeaORM 读 `{table_prefix}casbin_rule` 表，
//!   表名前缀由 `[authz].table_prefix` 运行时决定），支持策略热更新；
//! - [`Enforcer`]：装配入口 + `enforce(sub, obj, act)` 助手；
//!   `middleware/authz` 在认证之后按路由要求 obj/act 校验，未过返回 403。

pub mod adapter; // 导出策略存储适配子模块（file / db 两种来源）
pub mod model; // 导出内置 Casbin 模型模板子模块

mod enforcer; // Casbin 强制器（装配 + 校验 + 策略热更新）

pub use adapter::{casbin_table_name, CASBIN_TABLE_BASE}; // 表名基名 + 拼名助手（应用侧建表迁移复用同一规则）
pub use enforcer::Enforcer; // 对外导出授权强制器句柄
