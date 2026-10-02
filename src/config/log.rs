use serde::{Deserialize, Serialize};

fn default_level() -> String {
    "info".to_string()
}
fn default_format() -> String {
    "console".to_string()
}

/// `[log]` 配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    /// tracing 过滤语法，如 `info`、`info,sqlx=warn`；`RUST_LOG` 优先
    #[serde(default = "default_level")]
    pub level: String,
    /// console | json
    #[serde(default = "default_format")]
    pub format: String,
    /// 服务来源标识（可选，分布式部署用）：多服务日志进同一个采集后端时区分来源，
    /// 每条请求内日志自动携带。缺省回落 `[otel].service_name`，再回落 "core-rs"
    /// （见 [`crate::config::AppConfig::service_name`]）
    #[serde(default)]
    pub service_name: String,
    /// 是否同时输出到 stdout/stderr（默认 true）。`file.enabled = true` 时二者并存：
    /// 控制台给人看，文件归档给采集器
    #[serde(default = "default_true")]
    pub stdout: bool,
    /// 文件轮转输出（feature = "log-file"）
    #[serde(default)]
    pub file: FileLogConfig,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_level(),
            format: default_format(),
            service_name: String::new(),
            stdout: true,
            file: FileLogConfig::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// `[log.file]` 配置段（feature = "log-file"）：基于 rotate-rs 的滚动文件输出。
/// 支持按大小 / 按时间 / 混合三种轮转策略，轮转出的旧文件可保留 N 份并可选 gzip 压缩。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLogConfig {
    /// 是否启用文件输出；`RUST_LOG_FILE=1` 环境变量可强制开启
    #[serde(default)]
    pub enabled: bool,
    /// 日志目录（不存在自动创建）
    #[serde(default = "default_file_dir")]
    pub dir: String,
    /// 文件基础名（实际文件名形如 `app.2026-10-01-12-00-00.1.log`）
    #[serde(default = "default_file_name")]
    pub name: String,
    /// 轮转策略：size | time | hybrid（默认；任一条件满足即切割）
    #[serde(default = "default_rotation")]
    pub rotation: String,
    /// hybrid/size 策略的单文件大小上限（MB）
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u64,
    /// hybrid/time 策略的时间间隔（秒）
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    /// 保留的轮转文件数量上限（含最新一个）；0 = 不清理
    #[serde(default = "default_max_backups")]
    pub max_backups: usize,
    /// 轮转出的旧文件是否 gzip 压缩（后台异步完成）
    #[serde(default)]
    pub compress: bool,
    /// 非阻塞写入（默认 true）：后台线程落盘，磁盘卡顿时业务不被阻塞；
    /// false = 同步写（每条日志直接写文件，磁盘故障会拖慢请求）
    #[serde(default = "default_true")]
    pub non_blocking: bool,
    /// 非阻塞写入的溢出策略：block（背压等待，不丢日志）| drop（丢弃并计数，write 永不阻塞）
    #[serde(default = "default_overflow")]
    pub overflow: String,
}

impl Default for FileLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_file_dir(),
            name: default_file_name(),
            rotation: default_rotation(),
            max_size_mb: default_max_size_mb(),
            interval_secs: default_interval_secs(),
            max_backups: default_max_backups(),
            compress: false,
            non_blocking: true,
            overflow: default_overflow(),
        }
    }
}

fn default_file_dir() -> String {
    "logs".to_string()
}
fn default_file_name() -> String {
    "app".to_string()
}
fn default_rotation() -> String {
    "hybrid".to_string()
}
fn default_max_size_mb() -> u64 {
    100
}
fn default_interval_secs() -> u64 {
    86_400
}
fn default_max_backups() -> usize {
    30
}
fn default_overflow() -> String {
    "block".to_string()
}
