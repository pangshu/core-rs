//! `[log]` 配置节：级别、格式（文本/JSON）、输出目标（stdout / 滚动文件）。

use serde::{Deserialize, Serialize}; // 引入 serde 序列化/反序列化派生宏

fn default_level() -> String { // 默认日志级别取值函数
    "info".to_string() // 默认 info 级别
}
fn default_format() -> String { // 默认日志格式取值函数
    "console".to_string() // 默认 console 文本格式
}
fn default_true() -> bool { // 默认布尔开关取值函数
    true // 默认 true
}

#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct LogSettings { // 定义 `[log]` 配置结构体
    /// tracing 过滤语法，如 `info`、`info,sqlx=warn`；`RUST_LOG` 优先
    #[serde(default = "default_level")] // 缺省为 info
    pub level: String, // tracing 过滤语法
    /// console | json（JSON 便于采集进 ELK / Loki）
    #[serde(default = "default_format")] // 缺省为 console
    pub format: String, // 日志格式
    /// 服务来源标识（可选，分布式部署用）：进每条请求内日志的 `service` 字段
    #[serde(default)] // 缺省为空串
    pub service_name: String, // 服务来源标识
    /// 是否同时输出到 stdout/stderr（默认 true）；文件开启时二者并存
    #[serde(default = "default_true")] // 缺省为 true
    pub stdout: bool, // 是否输出到 stdout/stderr
    /// 滚动文件输出（feature = "log-file"）
    #[serde(default)] // 缺省用文件日志默认值
    pub file: FileLogSettings, // 滚动文件输出配置
}

impl Default for LogSettings { // 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            level: default_level(), // 默认 info
            format: default_format(), // 默认 console
            service_name: String::new(), // 默认无服务名
            stdout: default_true(), // 默认输出到 stdout
            file: FileLogSettings::default(), // 文件日志默认配置
        }
    }
}

/// `[log.file]`：基于 rotate-rs 的滚动文件输出（大小/时间/混合切割 + gz 压缩）。
#[derive(Debug, Clone, Serialize, Deserialize)] // 派生调试/克隆与 serde
pub struct FileLogSettings { // 定义 `[log.file]` 配置
    #[serde(default)] // 缺省为 false
    pub enabled: bool, // 是否启用文件输出
    /// 日志目录（不存在自动创建）
    #[serde(default = "default_file_dir")] // 缺省为 logs
    pub dir: String, // 日志目录
    /// 文件基础名
    #[serde(default = "default_file_name")] // 缺省为 app
    pub name: String, // 文件基础名
    /// 轮转策略：size | time | hybrid（默认；任一条件满足即切割）
    #[serde(default = "default_rotation")] // 缺省为 hybrid
    pub rotation: String, // 轮转策略
    /// hybrid/size 策略的单文件大小上限（MB）
    #[serde(default = "default_max_size_mb")] // 缺省为 100
    pub max_size_mb: u64, // 单文件大小上限（MB）
    /// hybrid/time 策略的时间间隔（秒）
    #[serde(default = "default_interval_secs")] // 缺省为 86400
    pub interval_secs: u64, // 时间间隔（秒）
    /// 保留的轮转文件数量上限（含最新一个）；0 = 不清理
    #[serde(default = "default_max_backups")] // 缺省为 30
    pub max_backups: usize, // 保留轮转文件数上限
    /// 轮转出的旧文件是否 gzip 压缩（后台异步完成）
    #[serde(default)] // 缺省为 false
    pub compress: bool, // 是否 gzip 压缩旧文件
    /// 非阻塞写入（默认 true）：后台线程落盘，磁盘卡顿时业务不被阻塞
    #[serde(default = "default_true")] // 缺省为 true
    pub non_blocking: bool, // 是否非阻塞写入
    /// 非阻塞写入的溢出策略：block（背压等待，不丢日志）| drop（丢弃并计数）
    #[serde(default = "default_overflow")] // 缺省为 block
    pub overflow: String, // 溢出策略
}

fn default_file_dir() -> String { // 目录默认值函数
    "logs".to_string() // 默认 logs
}
fn default_file_name() -> String { // 文件基础名默认值函数
    "app".to_string() // 默认 app
}
fn default_rotation() -> String { // 轮转策略默认值函数
    "hybrid".to_string() // 默认 hybrid
}
fn default_max_size_mb() -> u64 { // 单文件大小默认值函数
    100 // 默认 100MB
}
fn default_interval_secs() -> u64 { // 时间间隔默认值函数
    86_400 // 默认 86400 秒
}
fn default_max_backups() -> usize { // 备份数量默认值函数
    30 // 默认保留 30 个
}
fn default_overflow() -> String { // 溢出策略默认值函数
    "block".to_string() // 默认 block 背压
}

impl Default for FileLogSettings { // 手写默认值
    fn default() -> Self { // 实现 default 方法
        Self { // 构造默认实例
            enabled: false, // 默认关闭
            dir: default_file_dir(), // 默认 logs
            name: default_file_name(), // 默认 app
            rotation: default_rotation(), // 默认 hybrid
            max_size_mb: default_max_size_mb(), // 默认 100MB
            interval_secs: default_interval_secs(), // 默认 86400 秒
            max_backups: default_max_backups(), // 默认 30
            compress: false, // 默认不压缩
            non_blocking: true, // 默认非阻塞
            overflow: default_overflow(), // 默认 block
        }
    }
}
