//! `[database]` 配置节：DSN、池大小、慢查询阈值。
//!
//! url 为空表示不启用数据库；敏感项（密码）只走环境变量，不写入 toml。

use serde::{Deserialize, Serialize};

fn default_slow_ms() -> u64 {
    500
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct DatabaseSettings {
    #[serde(default)]
    pub url: String,
    /// 连接池上限，0 表示用底层默认值
    #[serde(default)]
    pub max_connections: u32,
    /// 连接池下限（预热连接，避免冷启动抖动），0 表示用底层默认值
    #[serde(default)]
    pub min_connections: u32,
    /// 从池中获取连接的超时（秒）；0 表示用底层默认值（30s）
    #[serde(default)]
    pub connect_timeout_secs: u64,
    /// 连接空闲回收时间（秒）；0 表示用底层默认值
    #[serde(default)]
    pub idle_timeout_secs: u64,
    /// 连接最长存活时间（秒）；0 表示用底层默认值
    #[serde(default)]
    pub max_lifetime_secs: u64,
    /// 开启后以 Debug 级别打印每条执行的 SQL（受 [log].level 过滤）
    #[serde(default)]
    pub sql_logging: bool,
    /// 慢 SQL 阈值（毫秒），超过以 Warn 级别记录；0 表示关闭
    #[serde(default = "default_slow_ms")]
    pub slow_query_ms: u64,
}

impl DatabaseSettings {
    /// 数据库是否已配置（url 非空）
    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }
}

/// 手写 Debug：连接串里的密码只走环境变量，但任何一处 `{:?}` 都会把它打进日志
impl std::fmt::Debug for DatabaseSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseSettings")
            .field("url", &super::redact_url(&self.url))
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .field("max_lifetime_secs", &self.max_lifetime_secs)
            .field("sql_logging", &self.sql_logging)
            .field("slow_query_ms", &self.slow_query_ms)
            .finish()
    }
}
