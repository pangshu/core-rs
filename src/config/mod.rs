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

pub mod env;
pub mod sections;
pub mod settings;
pub mod source;
#[cfg(feature = "watch")]
pub mod watch;

pub use env::Environment;
pub use sections::*;
pub use settings::{ConfigHandle, OnChange, Settings};
pub use source::{load, LoadOptions};
#[cfg(feature = "watch")]
pub use watch::{reload_once, Watcher};

use serde::de::DeserializeOwned;

/// 便捷加载：`load::<Settings>(&LoadOptions::new(env))` 的泛型入口
pub fn load_with<T: DeserializeOwned>(opts: &LoadOptions) -> Result<T, config::ConfigError> {
    source::load(opts)
}
