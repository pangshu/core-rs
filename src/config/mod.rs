//! 配置模块：单一入口 [`AppConfig::load`]。
//!
//! 加载优先级（后者覆盖前者）：内置默认值 → `app.yml` → `app-{profile}.yml`
//! （profile 取 `CORE_PROFILE` 环境变量或构建器 `.profile()`，env 优先）
//! → `CORE_` 前缀环境变量（`__` 表示层级，如 `CORE_SERVER__PORT=9090` 覆盖 `server.port`）。

mod cache;
mod datasource;
mod log;
mod queue;
mod redis;
#[cfg(feature = "jwt")]
mod security;
mod server;
mod watch;

#[cfg(feature = "otel")]
mod otel;
#[cfg(feature = "watch")]
pub mod hot_reload;

use config::{Config, Environment, File};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use cache::{CacheConfig, MemoryConfig};
pub use datasource::DatasourceConfig;
pub use log::{FileLogConfig, LogConfig};
pub use queue::{QueueConfig, QueueMemoryConfig, QueueRedisConfig};
pub use redis::RedisConfig;
pub use server::{CompressionConfig, CorsConfig, RateLimitConfig, ServerConfig};
pub use watch::WatchConfig;

#[cfg(feature = "jwt")]
pub use security::JwtConfig;
#[cfg(feature = "otel")]
pub use otel::OtelConfig;

/// 应用配置聚合结构，对应 app.yml 的顶层节点
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub datasource: DatasourceConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub redis: RedisConfig,
    #[serde(default)]
    pub log: LogConfig,
    /// `[watch]` 配置段：配置热更新开关
    #[serde(default)]
    pub watch: WatchConfig,
    /// `[queue]` 配置段（feature = "queue"）
    #[cfg(feature = "queue")]
    #[serde(default)]
    pub queue: QueueConfig,
    #[cfg(feature = "jwt")]
    #[serde(default)]
    pub jwt: JwtConfig,
    #[cfg(feature = "otel")]
    #[serde(default)]
    pub otel: OtelConfig,
    /// `[app]` 自定义配置段：框架不解析、原样保留，业务应用通过
    /// [`AppConfig::app_section`] 按 typed 结构取出（环境变量覆盖同样生效）
    #[serde(default)]
    pub app: Option<serde_json::Value>,
}

impl AppConfig {
    /// 从 yml + 环境变量加载配置。文件不存在且无环境变量时全部取默认值。
    ///
    /// profile 机制：`CORE_PROFILE=prod`（或调用方显式传入）时叠加加载
    /// `app-prod.yml`，其中的键覆盖基础文件；env 变量优先于显式传入值。
    pub fn load(path: Option<&str>) -> Result<Self, config::ConfigError> {
        Self::load_with_profile(path, None)
    }

    /// 同 [`AppConfig::load`]，但允许代码层显式指定 profile（`CORE_PROFILE` 优先）。
    pub fn load_with_profile(
        path: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Self, config::ConfigError> {
        let path = path.unwrap_or("app.yml");
        let profile = std::env::var("CORE_PROFILE")
            .ok()
            .filter(|p| !p.is_empty())
            .or_else(|| profile.map(|p| p.to_string()));

        let built = build_config(path, profile.as_deref())?;
        // 此时 tracing 尚未初始化，未知键告警直接走 stderr（启动阶段的一次性输出）
        warn_unknown_keys(build_config(path, profile.as_deref())?);
        built.try_deserialize()
    }

    /// 日志与追踪用的服务标识，回退链：`[log].service_name` → `[otel].service_name` → "core-rs"。
    /// 进请求 span 的 `service` 字段（每条请求内日志携带），otel 导出时也用作资源名兜底。
    pub fn service_name(&self) -> &str {
        if !self.log.service_name.is_empty() {
            return &self.log.service_name;
        }
        #[cfg(feature = "otel")]
        if !self.otel.service_name.is_empty() {
            return &self.otel.service_name;
        }
        "core-rs"
    }

    /// 取 `[app]` 自定义配置段并反序列化为应用自己的结构。
    /// 段不存在时返回 `None`，配合 `unwrap_or_default()` 使用：
    ///
    /// ```no_run
    /// # use core_rs::config::AppConfig;
    /// # use core_rs::prelude::*;
    /// # #[derive(Deserialize, Default)]
    /// # struct MyCfg { timeout_secs: u64 }
    /// # fn demo(cfg: &AppConfig) -> Result<(), core_rs::error::AppError> {
    /// let my: MyCfg = cfg.app_section::<MyCfg>()?.unwrap_or_default();
    /// # Ok(())
    /// # }
    /// ```
    pub fn app_section<T: DeserializeOwned>(&self) -> Result<Option<T>, AppSectionError> {
        match &self.app {
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(AppSectionError),
            None => Ok(None),
        }
    }
}

/// 组装配置源：yml → profile 覆盖文件 → CORE_ 环境变量（后者优先）
fn build_config(
    path: &str,
    profile: Option<&str>,
) -> Result<Config, config::ConfigError> {
    let mut builder = Config::builder().add_source(File::with_name(path).required(false));
    if let Some(p) = profile {
        builder = builder.add_source(
            // app.yml → app-prod.yml（保留显式扩展名）；文件不存在时静默跳过
            File::with_name(&profile_file_name(path, p)).required(false),
        );
    }
    builder
        .add_source(
            // CORE_LOG__LEVEL=debug 覆盖 log.level；prefix_separator 必须显式指定，
            // 否则会跟随 separator("__") 导致 CORE_ 前缀匹配不上
            Environment::with_prefix("CORE")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()
}

/// 顶层已知配置键（`profile` 来自 CORE_PROFILE 环境变量的透传）
#[allow(unused_mut)] // 部分可选 feature 关闭时无追加项
fn known_top_level_keys() -> Vec<&'static str> {
    let mut known = vec![
        "server",
        "datasource",
        "cache",
        "redis",
        "log",
        "watch",
        "app",
        "profile",
    ];
    #[cfg(feature = "queue")]
    known.push("queue");
    #[cfg(feature = "jwt")]
    known.push("jwt");
    #[cfg(feature = "otel")]
    known.push("otel");
    known
}

/// 未知顶层键告警：拼写错误（如 slow_query 写成 slow_query）会静默失效，启动时提示
fn warn_unknown_keys(config: Config) {
    let Ok(all) = config.try_deserialize::<serde_json::Value>() else {
        return;
    };
    let Some(obj) = all.as_object() else {
        return;
    };
    let known = known_top_level_keys();
    for key in obj.keys() {
        if !known.contains(&key.to_ascii_lowercase().as_str()) {
            eprintln!("core-rs config: 未知的顶层配置键 `{key}`（拼写错误？该键将被忽略）");
        }
    }
}

/// profile 文件名：`<dir>/app.yml` + `prod` → `<dir>/app-prod.yml`（保留父目录与显式扩展名）
fn profile_file_name(path: &str, profile: &str) -> String {
    let p = std::path::Path::new(path);
    match (p.file_stem(), p.extension()) {
        (Some(stem), Some(ext)) => p
            .with_file_name(format!(
                "{}-{}.{}",
                stem.to_string_lossy(),
                profile,
                ext.to_string_lossy()
            ))
            .to_string_lossy()
            .to_string(),
        (Some(stem), None) => p
            .with_file_name(format!("{}-{}", stem.to_string_lossy(), profile))
            .to_string_lossy()
            .to_string(),
        _ => format!("{path}-{profile}"),
    }
}

/// `[app]` 段与应用结构不匹配（缺字段/类型不对等）时的错误
#[derive(Debug, thiserror::Error)]
#[error("[app] config section does not match the target struct: {0}")]
pub struct AppSectionError(#[from] serde_json::Error);
