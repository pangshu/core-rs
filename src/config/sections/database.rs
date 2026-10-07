//! `[database]` 配置节：DSN、池大小、慢查询阈值。
//!
//! url 为空表示不启用数据库；敏感项（密码）只走环境变量，不写入 toml。

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

fn default_slow_ms() -> u64 { // 慢查询阈值默认值函数
    500 // 默认 500 毫秒
}

#[derive(Clone, Default, Serialize, Deserialize)] // 派生克隆/默认值与 serde（Debug 手写）
pub struct DatabaseSettings { // 定义 `[database]` 配置结构体
    #[serde(default)] // 缺省为空串（不启用数据库）
    pub url: String, // 数据库连接串
    /// 连接池上限，0 表示用底层默认值
    #[serde(default)] // 缺省为 0（用底层默认）
    pub max_connections: u32, // 连接池上限
    /// 连接池下限（预热连接，避免冷启动抖动），0 表示用底层默认值
    #[serde(default)] // 缺省为 0（用底层默认）
    pub min_connections: u32, // 连接池下限
    /// 从池中获取连接的超时（秒）；0 表示用底层默认值（30s）
    #[serde(default)] // 缺省为 0（用底层默认 30s）
    pub connect_timeout_secs: u64, // 获取连接超时（秒）
    /// 连接空闲回收时间（秒）；0 表示用底层默认值
    #[serde(default)] // 缺省为 0（用底层默认）
    pub idle_timeout_secs: u64, // 空闲回收时间（秒）
    /// 连接最长存活时间（秒）；0 表示用底层默认值
    #[serde(default)] // 缺省为 0（用底层默认）
    pub max_lifetime_secs: u64, // 连接最长存活时间（秒）
    /// 开启后以 Debug 级别打印每条执行的 SQL（受 [log].level 过滤）
    #[serde(default)] // 缺省为 false
    pub sql_logging: bool, // 是否打印执行的 SQL
    /// 慢 SQL 阈值（毫秒），超过以 Warn 级别记录；0 表示关闭
    #[serde(default = "default_slow_ms")] // 缺省为 500ms
    pub slow_query_ms: u64, // 慢 SQL 阈值（毫秒）
}

impl DatabaseSettings { // 为 DatabaseSettings 实现方法
    /// 数据库是否已配置（url 非空）
    pub fn enabled(&self) -> bool { // 判断数据库是否已配置
        !self.url.is_empty() // url 非空即视为启用
    }
}

/// 手写 Debug：连接串里的密码只走环境变量，但任何一处 `{:?}` 都会把它打进日志
impl std::fmt::Debug for DatabaseSettings { // 手写 Debug，避免泄露连接串密码
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现 fmt 方法
        f.debug_struct("DatabaseSettings") // 开始构造调试输出
            .field("url", &super::redact_url(&self.url)) // url 字段经脱敏后输出
            .field("max_connections", &self.max_connections) // 输出连接池上限
            .field("min_connections", &self.min_connections) // 输出连接池下限
            .field("connect_timeout_secs", &self.connect_timeout_secs) // 输出连接超时
            .field("idle_timeout_secs", &self.idle_timeout_secs) // 输出空闲回收时间
            .field("max_lifetime_secs", &self.max_lifetime_secs) // 输出最长存活时间
            .field("sql_logging", &self.sql_logging) // 输出 SQL 日志开关
            .field("slow_query_ms", &self.slow_query_ms) // 输出慢查询阈值
            .finish() // 结束并生成调试输出
    }
}
