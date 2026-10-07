//! `[task]` 配置节：定时任务开关、分布式锁开关、任务列表（文档 三·19）。

use serde::{Deserialize, Serialize}; // 引入 serde 反序列化/序列化派生宏

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 能力
pub struct TaskSettings { // 定时任务配置结构
    #[serde(default)] // 缺失时用默认值
    pub enabled: bool, // 是否启用定时任务
    /// 多实例防重复：经 cache/lock 选主，保证同一时刻仅一个实例执行
    #[serde(default)] // 缺失时用默认值
    pub distributed_lock: bool, // 是否启用分布式锁防重复执行
    /// 分布式锁 TTL（秒）；单轮执行超过该时长的任务请自行延长
    #[serde(default = "default_lock_ttl_secs")] // 缺失时用默认 TTL
    pub lock_ttl_secs: u64, // 分布式锁持有时长（秒）
    /// 任务声明列表：代码里按 name 注册 handler，配置里按 name 启用/覆盖参数。
    /// 不在此列表的任务不运行。
    #[serde(default)] // 缺失时用默认值
    pub jobs: Vec<JobSettings>, // 定时任务声明列表
}

fn default_lock_ttl_secs() -> u64 { // 锁 TTL 的默认值函数
    600 // 默认 600 秒
}

impl Default for TaskSettings { // 为定时任务配置实现 Default
    fn default() -> Self { // 返回默认配置
        Self { // 构造默认配置
            enabled: false, // 默认不启用定时任务
            distributed_lock: false, // 默认不启用分布式锁
            lock_ttl_secs: default_lock_ttl_secs(), // 默认锁 TTL
            jobs: Vec::new(), // 默认无任务
        }
    }
}

/// 单个定时任务的配置声明
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde 能力
pub struct JobSettings { // 单个定时任务配置结构
    /// 与代码注册的 handler 名对应
    pub name: String, // 任务名，用于匹配已注册的 handler
    /// cron 表达式，6/7 段（秒开头），如 `0/30 * * * * *`
    pub schedule: String, // cron 调度表达式
    /// IANA 时区名（如 Asia/Shanghai）；缺省用 UTC
    #[serde(default)] // 缺失时用默认值
    pub timezone: String, // 任务执行时区
}

impl JobSettings { // 为任务配置实现构造方法
    pub fn new(name: impl Into<String>, schedule: impl Into<String>) -> Self { // 用任务名与 cron 表达式构造
        Self { // 构造任务配置
            name: name.into(), // 保存任务名
            schedule: schedule.into(), // 保存 cron 表达式
            timezone: String::new(), // 默认空时区（用 UTC）
        }
    }
}
