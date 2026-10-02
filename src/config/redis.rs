use serde::{Deserialize, Serialize};

/// `[redis]` 配置段。url 为空表示不启用缓存（`Cache` 提取器将报内部错误）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedisConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub pool_size: u32,
}
