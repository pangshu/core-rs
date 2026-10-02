use serde::{Deserialize, Serialize};

fn default_slow_ms() -> u64 {
    500
}

/// `[datasource]` 配置段。url 为空表示不启用数据库（`Db` 提取器将报内部错误）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatasourceConfig {
    #[serde(default)]
    pub url: String,
    /// 连接池上限，0 表示用底层默认值
    #[serde(default)]
    pub max_connections: u32,
    /// 连接池下限（预热连接，避免冷启动抖动），0 表示用底层默认值
    #[serde(default)]
    pub min_connections: u32,
    /// 从池中获取连接的超时时间（秒），超时常见于池耗尽/慢查询堆积；0 表示用底层默认值（30s）
    #[serde(default)]
    pub connect_timeout_secs: u64,
    /// 连接空闲回收时间（秒）；0 表示用底层默认值
    #[serde(default)]
    pub idle_timeout_secs: u64,
    /// 连接最长存活时间（秒），配合数据库端连接回收（如 MySQL wait_timeout）；0 表示用底层默认值
    #[serde(default)]
    pub max_lifetime_secs: u64,
    /// 开启后以 Debug 级别打印每条执行的 SQL（经 tracing，受 log.level 控制）
    #[serde(default)]
    pub sql_logging: bool,
    /// 慢 SQL 阈值（毫秒），超过以 Warn 级别记录；0 表示关闭
    #[serde(default = "default_slow_ms")]
    pub slow_query_ms: u64,
}
