//! `[task]` 配置节：定时任务开关、分布式锁开关、任务列表（文档 三·19）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 多实例防重复：经 cache/lock 选主，保证同一时刻仅一个实例执行
    #[serde(default)]
    pub distributed_lock: bool,
    /// 分布式锁 TTL（秒）；单轮执行超过该时长的任务请自行延长
    #[serde(default = "default_lock_ttl_secs")]
    pub lock_ttl_secs: u64,
    /// 任务声明列表：代码里按 name 注册 handler，配置里按 name 启用/覆盖参数。
    /// 不在此列表的任务不运行。
    #[serde(default)]
    pub jobs: Vec<JobSettings>,
}

fn default_lock_ttl_secs() -> u64 {
    600
}

impl Default for TaskSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            distributed_lock: false,
            lock_ttl_secs: default_lock_ttl_secs(),
            jobs: Vec::new(),
        }
    }
}

/// 单个定时任务的配置声明
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSettings {
    /// 与代码注册的 handler 名对应
    pub name: String,
    /// cron 表达式，6/7 段（秒开头），如 `0/30 * * * * *`
    pub schedule: String,
    /// IANA 时区名（如 Asia/Shanghai）；缺省用 UTC
    #[serde(default)]
    pub timezone: String,
}

impl JobSettings {
    pub fn new(name: impl Into<String>, schedule: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            schedule: schedule.into(),
            timezone: String::new(),
        }
    }
}
