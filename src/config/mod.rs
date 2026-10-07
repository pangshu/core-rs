//! 配置子系统：多环境 + 集中解析 + 热更新（文档 三·4）。
//!
//! - **多环境**：`APP_ENV` 决定加载哪份；合并顺序
//!   `default.toml → {env}.toml → 环境变量 →（可选）配置中心`，靠后者覆盖前者；
//! - **集中解析**：所有子系统配置结构集中在 [`sections`]，由 [`Settings`] 聚合成唯一根；
//! - **热更新**：加载后对外只给 [`ConfigHandle`]（`ArcSwap` 只读句柄，读取零锁），
//!   文件/远端变更重载校验通过后原子替换，失败保留旧值（fail-safe）。
//!
//! ```no_run
//! use core_rs::config::{self, Environment, LoadOptions};
//!
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! // 框架配置（各模块均引用 config::sections::*）
//! let settings: config::Settings = config::load(&LoadOptions::new(Environment::from_env()))?;
//! let _ = settings.server.port;
//!
//! // 应用在 flatten 框架根之上追加业务节后，同样一行加载：
//! // let app: AppSettings = config::load(&LoadOptions::new(Environment::from_env()))?;
//! # Ok(())
//! # }
//! ```

pub mod env; // 声明运行环境（APP_ENV）子模块
pub mod sections; // 声明各子系统配置节子模块
pub mod settings; // 声明配置根结构与只读句柄子模块
pub mod source; // 声明配置来源链（合并+反序列化）子模块
#[cfg(feature = "watch")] // 仅在开启 watch feature 时编译下面的模块声明
pub mod watch; // 声明配置热更新监听子模块

pub use env::Environment; // 对外导出运行环境枚举
pub use sections::*; // 对外导出全部配置节结构
pub use settings::{ConfigHandle, OnChange, Settings}; // 对外导出配置根、只读句柄与变更回调类型
pub use source::{load, LoadOptions}; // 对外导出加载函数与加载选项
#[cfg(feature = "watch")] // 仅在开启 watch feature 时编译下面的重导出
pub use watch::{reload_once, Watcher}; // 对外导出热更新监听器与单次重载函数

use serde::de::DeserializeOwned; // 引入可反序列化 trait，作为泛型加载约束

/// 便捷加载：`load::<Settings>(&LoadOptions::new(env))` 的泛型入口
pub fn load_with<T: DeserializeOwned>(opts: &LoadOptions) -> Result<T, config::ConfigError> { // 泛型便捷加载函数，返回目标配置类型
    source::load(opts) // 委托给 source::load 完成来源组装与反序列化
}
