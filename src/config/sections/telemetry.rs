//! `[telemetry]` 配置段。

use std::time::Duration;

use byte_unit::Byte;
use serde::Deserialize;

use crate::config::sections::de::{de_byte, de_duration};
use crate::telemetry::format::LogFormat;

/// 可观测性配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct TelemetrySection {
    /// 日志级别过滤表达式（`RUST_LOG` 风格，如 `info`、`aurora=debug`）。
    pub level: String,
    /// 日志输出格式。
    pub format: LogFormat,
    /// 是否输出到 stdout（文件日志是**叠加**，不替换控制台）。
    pub stdout: bool,
    /// 文件日志（feature `log-file`）。
    pub file: FileSection,
    /// 指标（M6）。
    pub metrics: MetricsSection,
    /// 追踪导出（M6）。
    pub otel: OtelSection,
}

impl Default for TelemetrySection {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Json,
            stdout: true,
            file: FileSection::default(),
            metrics: MetricsSection::default(),
            otel: OtelSection::default(),
        }
    }
}

impl TelemetrySection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// 指标端点路径非法、OTel 开启但缺少端点 / 服务名时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// `production = true` 时额外要求：启用 `/metrics` 就必须配抓取令牌，
    /// 否则指标端点等于对公网开放（含 QPS、错误率、连接数等运行态信息）。
    ///
    /// # Errors
    /// 上述条件不满足时返回错误。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        // 生产环境禁止把日志整体关掉：出事时「恰好」没有日志，等于事后无法取证，
        // 而且这类变更通常是为了「降噪」，没人会记得再打开。
        let level = self.level.trim();
        if production && level.eq_ignore_ascii_case("off") {
            return Err(
                "[telemetry].level = \"off\" 在生产环境不允许：日志是告警与事后取证的唯一线索"
                    .to_string(),
            );
        }
        if production && level_has_trace(level) {
            tracing::warn!(
                level = %level,
                "[telemetry].level 在生产环境开到 trace：日志体积会显著放大，且可能记录请求细节"
            );
        }
        if self.metrics.enabled {
            validate_metrics_path(&self.metrics.path)?;
            if self.metrics.token.trim().is_empty() {
                if production {
                    return Err(
                        "[telemetry].metrics.token 在生产环境必填（AURORA_TELEMETRY__METRICS__TOKEN）：\
                         否则 /metrics 无需认证即可抓取"
                            .to_string(),
                    );
                }
                tracing::warn!(
                    "[telemetry].metrics.enabled = true 但未设置 token：/metrics 无鉴权，\
                     仅可依赖网络隔离"
                );
            }
        }
        if self.otel.enabled {
            let endpoint = self.otel.endpoint.as_deref().map(str::trim);
            if endpoint.is_none_or(str::is_empty) {
                return Err(
                    "[telemetry].otel.enabled = true 但未配置 endpoint（如 http://localhost:4317）"
                        .to_string(),
                );
            }
            if self.otel.service_name.trim().is_empty() {
                return Err("[telemetry].otel.service_name 不能为空".to_string());
            }
            if !self.otel.sample_ratio.is_finite() || !(0.0..=1.0).contains(&self.otel.sample_ratio)
            {
                return Err(format!(
                    "[telemetry].otel.sample_ratio = {} 越界：必须在 0.0–1.0 之间",
                    self.otel.sample_ratio
                ));
            }
            // 明文出网：日志/span 可能含 PII，跨网段的 http 端点等于裸奔
            if let Some(endpoint) = endpoint.filter(|value| !value.is_empty()) {
                let scheme = endpoint_scheme(endpoint);
                let host = endpoint_host(endpoint);
                let plaintext = scheme.eq_ignore_ascii_case("http");
                if !plaintext && !scheme.eq_ignore_ascii_case("https") {
                    return Err(format!(
                        "[telemetry].otel.endpoint = \"{endpoint}\" 的 scheme 不受支持（只认 http/https）"
                    ));
                }
                if plaintext && !is_loopback_host(host) && !self.otel.allow_insecure {
                    return Err(format!(
                        "[telemetry].otel.endpoint = \"{endpoint}\" 是明文 http 且非本机：\
                         追踪与日志会以明文跨网传输。改用 https，或显式设置 \
                         [telemetry].otel.allow_insecure = true"
                    ));
                }
            }
        }
        // 注意：这一条必须在 `enabled` 分支**外**，否则条件恒假
        if self.otel.logs && !self.otel.enabled {
            return Err("[telemetry].otel.logs 需要同时设置 otel.enabled = true".to_string());
        }
        // 日志至少要有一个出口：`stdout = false` 且文件日志没开时，level 配得再细
        // 也没人看得到——这种「静默无日志」必须在启动期拒掉。
        if !self.stdout && !self.file.enabled {
            return Err(
                "[telemetry].stdout = false 且 [telemetry].file.enabled = false：\
                 日志没有任何输出目的地"
                    .to_string(),
            );
        }
        self.validate_file(production)?;
        Ok(())
    }

    /// 文件日志（`[telemetry].file`）校验。
    ///
    /// rotate-rs 在 `open()` 里也会校验（分隔符、日期格式、模板），但那已经是
    /// telemetry 初始化阶段，错误文案是库的。这里先在配置层拦一遍，让
    /// 「配置写错」与「目录/权限问题」在启动错误里能区分开。
    fn validate_file(&self, production: bool) -> Result<(), String> {
        let file = &self.file;
        if !file.enabled {
            return Ok(());
        }
        // 构建里没编进这个 feature 时不能静默不落盘：配置写了就必须真的生效。
        if !cfg!(feature = "log-file") {
            return Err(
                "[telemetry].file.enabled = true 需要启用 feature \"log-file\"：\
                 当前构建未编译文件日志，日志不会落盘"
                    .to_string(),
            );
        }

        let dir = file.dir.trim();
        if dir.is_empty() {
            return Err("[telemetry].file.dir 不能为空".to_string());
        }
        if dir.len() > 512 {
            return Err("[telemetry].file.dir 过长（上限 512 字节）".to_string());
        }
        if let Some(bad) = dir.chars().find(|c| c.is_control()) {
            return Err(format!("[telemetry].file.dir 含控制字符 {bad:?}"));
        }
        if dir.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(format!(
                "[telemetry].file.dir = \"{dir}\" 含 `..` 路径段：日志目录必须显式给出，不做路径穿越"
            ));
        }
        // 文件名组件：rotate-rs 也有同样约束，但错误要能指向具体配置键
        for (key, value) in [("name", &file.name), ("suffix", &file.suffix)] {
            if value.contains(['/', '\\']) {
                return Err(format!(
                    "[telemetry].file.{key} = {value:?} 不能包含路径分隔符"
                ));
            }
            if value.chars().any(char::is_control) {
                return Err(format!("[telemetry].file.{key} 含控制字符"));
            }
        }
        if file.name.is_empty() {
            return Err("[telemetry].file.name 不能为空".to_string());
        }

        let has_size = matches!(file.rotation, FileRotation::Size | FileRotation::Mixed);
        let has_time = matches!(file.rotation, FileRotation::Time | FileRotation::Mixed);
        let max_size = file.max_size.as_u64();
        if has_size && max_size == 0 {
            return Err(format!(
                "[telemetry].file.max_size = 0：rotation = {:?} 时必须给出单文件上限",
                file.rotation
            ));
        }
        if max_size > usize::MAX as u64 {
            return Err(format!(
                "[telemetry].file.max_size = {max_size} 超出当前平台的 usize 上限"
            ));
        }
        if has_time && file.interval < Duration::from_secs(1) {
            return Err(format!(
                "[telemetry].file.interval = {:?}：rotation = {:?} 时至少 1s",
                file.interval, file.rotation
            ));
        }
        if file.max_files > MAX_FILE_BACKUPS {
            return Err(format!(
                "[telemetry].file.max_files = {} 过大（上限 {MAX_FILE_BACKUPS}）",
                file.max_files
            ));
        }
        if production && file.max_files == 0 {
            return Err(
                "[telemetry].file.max_files = 0（不清理归档）在生产环境不允许：\
                 归档会无限增长直到把磁盘写满"
                    .to_string(),
            );
        }

        // —— 以下只告警 ——
        if file.max_files == 0 {
            tracing::warn!("[telemetry].file.max_files = 0：归档不清理，磁盘占用只增不减");
        } else if file.max_files < 3 {
            tracing::warn!(
                max_files = file.max_files,
                "[telemetry].file.max_files 过小：轮转几次就开始删历史归档"
            );
        }
        if matches!(file.mode, FileWriteMode::Sync) && has_time {
            tracing::warn!(
                "[telemetry].file.mode = \"sync\"：同步模式的时间轮转依赖写入触发，\
                 进程空闲时不会切割；需要准点切割请用 \"nonblocking\""
            );
        }
        if matches!(file.overflow, FileOverflow::Block) {
            tracing::warn!(
                "[telemetry].file.overflow = \"block\"：待写额度满时会阻塞调用线程，\
                 而 tracing 的调用点在 tokio worker 上——磁盘卡顿会传导成请求延迟"
            );
        }
        if production && !file.compress {
            tracing::warn!("[telemetry].file.compress = false：归档不压缩，磁盘占用会放大数倍");
        }
        if self.format != LogFormat::Json {
            tracing::warn!(
                "[telemetry].format 不是 json：文件日志同样是多行/非结构化格式，不利于采集"
            );
        }
        Ok(())
    }
}

/// 表达式里是否有指令把级别开到 `trace`（`trace` 或 `target=trace`）。
fn level_has_trace(level: &str) -> bool {
    level.split(',').any(|directive| {
        let directive = directive.trim();
        if directive.eq_ignore_ascii_case("trace") {
            return true;
        }
        directive
            .rsplit_once('=')
            .is_some_and(|(_, value)| value.trim().eq_ignore_ascii_case("trace"))
    })
}

/// 校验指标端点路径的**形状**。
///
/// 只校验「以 `/` 开头」是不够的：该值直接进 `Router::route`，含 `?` / `#` / `*` /
/// `{` 等字符会让 axum 在装配期 panic（而不是给出可读的启动错误）。同时拒绝 `/`
/// 与 `..`、`//`——前者会把指标挂在根路径，后者不是合法路由模板。
fn validate_metrics_path(raw: &str) -> Result<(), String> {
    let path = raw.trim();
    if !path.starts_with('/') {
        return Err(format!(
            "[telemetry].metrics.path = \"{raw}\" 必须以 '/' 开头"
        ));
    }
    if path == "/" {
        return Err(
            "[telemetry].metrics.path 不能是根路径 \"/\"（会与业务根路由冲突）".to_string(),
        );
    }
    if path.len() > 128 {
        return Err("[telemetry].metrics.path 过长（上限 128 字节）".to_string());
    }
    if let Some(bad) = path.chars().find(|c| {
        matches!(c, '?' | '#' | '*' | '{' | '}' | '%') || c.is_whitespace() || c.is_control()
    }) {
        return Err(format!(
            "[telemetry].metrics.path = \"{raw}\" 含非法字符 {bad:?}：\
             必须是静态路径（不支持查询串 / 通配 / 路径参数）"
        ));
    }
    if path.contains("..") || path.contains("//") {
        return Err(format!(
            "[telemetry].metrics.path = \"{raw}\" 含非法路径段（`..` 或 `//`）"
        ));
    }
    Ok(())
}

/// 滚动文件日志配置段（feature `log-file`）。
///
/// 语义与 rotate-rs 对齐：活跃文件恒为 `{dir}/{name}.{suffix}`，轮转后归档到
/// `{name}.{date}.{suffix}`（同一秒内二次轮转补 `.1` / `.2`），`compress = true`
/// 时后台把归档压成 `.gz`。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct FileSection {
    /// 是否输出到文件（与 `[telemetry].stdout` 并存，不是替换它）。
    pub enabled: bool,
    /// 日志目录（相对当前工作目录或绝对路径；不存在会自动创建）。
    pub dir: String,
    /// 文件基础名。
    pub name: String,
    /// 文件后缀（前导 `.` 会被去掉）。
    pub suffix: String,
    /// 滚动策略。
    pub rotation: FileRotation,
    /// `size` / `mixed` 策略的单文件上限。
    #[serde(deserialize_with = "de_byte")]
    pub max_size: Byte,
    /// `time` / `mixed` 策略的间隔（**距上次轮转起算**，不对齐时钟边界）。
    #[serde(deserialize_with = "de_duration")]
    pub interval: Duration,
    /// 归档保留数量（含最新一个）；`0` = 不清理。
    pub max_files: u32,
    /// 归档是否 gzip 压缩（后台异步完成）。
    pub compress: bool,
    /// 写入模式。
    pub mode: FileWriteMode,
    /// 非阻塞模式的 channel 条数容量；`0` = 用库默认值（128K 条）。
    pub channel_capacity: usize,
    /// 非阻塞模式的待写字节配额；`0` = 用库默认值（256MB）。
    #[serde(deserialize_with = "de_byte")]
    pub max_pending_bytes: Byte,
    /// 额度满时的行为。
    pub overflow: FileOverflow,
}

impl Default for FileSection {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "logs".to_string(),
            name: "app".to_string(),
            suffix: "log".to_string(),
            rotation: FileRotation::Mixed,
            max_size: Byte::from_u64(100 * 1000 * 1000),
            interval: Duration::from_secs(24 * 60 * 60),
            max_files: 7,
            compress: true,
            mode: FileWriteMode::Nonblocking,
            channel_capacity: 0,
            max_pending_bytes: Byte::from_u64(0),
            overflow: FileOverflow::Drop,
        }
    }
}

/// `max_files` 上限：防手滑写成天文数字后在 `usize` 转换/建表时出问题。
const MAX_FILE_BACKUPS: u32 = 10_000;

/// 文件滚动策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FileRotation {
    /// 按大小滚动。
    Size,
    /// 按时间滚动（固定间隔，不校准时钟边界）。
    Time,
    /// 大小 + 时间，任一满足即滚动（默认）。
    #[default]
    #[serde(alias = "hybrid")]
    Mixed,
}

/// 文件写入模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FileWriteMode {
    /// 同步写：写盘在调用线程内完成，且**时间轮转依赖写入触发**（空闲不切割）。
    Sync,
    /// 非阻塞写：channel + 后台 worker，时间轮转由 worker 定时器驱动。
    #[default]
    #[serde(alias = "non-blocking")]
    Nonblocking,
}

/// 非阻塞写入额度满时的溢出策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FileOverflow {
    /// 丢弃新日志（write 永不阻塞；丢弃数在关闭时可观测）。
    #[default]
    Drop,
    /// 背压等待（不丢日志，但会阻塞调用线程）。
    Block,
}

/// 指标配置段（M6）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct MetricsSection {
    /// 是否启用指标。
    pub enabled: bool,
    /// 指标端点路径。
    pub path: String,
    /// 抓取令牌（🔒 只走环境变量 `AURORA_TELEMETRY__METRICS__TOKEN`）。
    ///
    /// 非空时 `/metrics` 要求 `Authorization: Bearer <token>`；为空则不做校验
    /// （此时**必须**靠网络隔离，见 `docs/12` §9）。
    pub token: String,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/metrics".to_string(),
            token: String::new(),
        }
    }
}

/// 追踪导出配置段（M6）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct OtelSection {
    /// 是否导出追踪。
    pub enabled: bool,
    /// OTLP 端点。
    pub endpoint: Option<String>,
    /// 服务名。
    pub service_name: String,
    /// 是否把日志同时导出为 OTel 日志（OTLP logs）。
    ///
    /// 开启后同一份 `tracing` 事件既写本地（fmt），也进 OTLP 后端，
    /// 便于在 Grafana / Loki / Tempo 里按 trace-id 关联日志与链路。
    pub logs: bool,
    /// 采样比例（0.0–1.0）。默认 1.0（全采）。
    ///
    /// 生产环境应按 QPS 下调：100% 采样会把 collector 与网络带宽打满，
    /// 而追踪本来就是「抽样看链路」，不需要全量。
    pub sample_ratio: f64,
    /// 允许**明文**（`http://`）OTLP 端点。默认 false。
    ///
    /// 开启 `otel.logs` 时日志（可能含 PII）会整体外发，明文跨网段等于裸奔；
    /// 只有 loopback 端点自动豁免。
    pub allow_insecure: bool,
}

impl Default for OtelSection {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            service_name: "aurora-app".to_string(),
            logs: false,
            sample_ratio: 1.0,
            allow_insecure: false,
        }
    }
}

/// 解析端点里的 scheme（小写）。
fn endpoint_scheme(endpoint: &str) -> &str {
    endpoint.split_once("://").map_or("", |(scheme, _)| scheme)
}

/// 解析端点里的主机名（去 userinfo / 端口 / 路径）。
fn endpoint_host(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // `[::1]:4317`
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or(bracketed);
    }
    host_port
        .rsplit_once(':')
        .map_or(host_port, |(host, _port)| host)
}

/// 端点是否指向本机（`localhost` / `127.0.0.0/8` / `::1`）。
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_validate_metrics_path() {
        let mut s = TelemetrySection::default();
        s.metrics.enabled = true;
        s.metrics.path = "metrics".to_string();
        assert!(s.validate().is_err());

        s.metrics.path = "/metrics".to_string();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_default_otel_logs_off() {
        assert!(!TelemetrySection::default().otel.logs);
    }

    #[test]
    fn should_require_otel_enabled_for_logs() {
        let mut s = TelemetrySection::default();
        s.otel.logs = true;
        assert!(s.validate().is_err(), "未启用 otel 却开 logs 应报错");

        s.otel.enabled = true;
        s.otel.endpoint = Some("http://localhost:4317".to_string());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_require_otel_endpoint_when_enabled() {
        let mut s = TelemetrySection::default();
        s.otel.enabled = true;
        assert!(s.validate().is_err());

        s.otel.endpoint = Some("http://localhost:4317".to_string());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_default_to_json_format() {
        assert_eq!(TelemetrySection::default().format, LogFormat::Json);
    }

    #[test]
    fn should_require_metrics_token_in_production() {
        let mut s = TelemetrySection::default();
        s.metrics.enabled = true;
        // 非生产：只告警
        assert!(s.validate_for_env(false).is_ok());
        // 生产：无令牌必须拒绝
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("metrics.token"), "实际：{error}");

        s.metrics.token = "scrape-token".to_string();
        assert!(s.validate_for_env(true).is_ok());
    }

    #[test]
    fn should_reject_metrics_path_shapes_that_break_router() {
        let mut s = TelemetrySection::default();
        s.metrics.enabled = true;
        // 这些值直接进 `Router::route`，会让装配期 panic 而不是给出可读错误
        for bad in [
            "/metrics?debug=1",
            "/metrics#frag",
            "/m/*",
            "/m/{name}",
            "/",
            "/a/../b",
            "/a//b",
            "/m etrics",
        ] {
            s.metrics.path = bad.to_string();
            assert!(s.validate().is_err(), "应拒绝 metrics.path = {bad:?}");
        }
        s.metrics.path = "/metrics".to_string();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_require_a_log_sink() {
        let s = TelemetrySection {
            stdout: false,
            ..TelemetrySection::default()
        };
        let error = s.validate().unwrap_err();
        assert!(error.contains("没有任何输出目的地"), "实际：{error}");
    }

    /// 未编译 `log-file` 时不能静默不落盘（配置写了却没生效）。
    #[cfg(not(feature = "log-file"))]
    #[test]
    fn should_require_log_file_feature() {
        let mut s = TelemetrySection::default();
        s.file.enabled = true;
        let error = s.validate().unwrap_err();
        assert!(error.contains("log-file"), "实际：{error}");
    }

    #[cfg(feature = "log-file")]
    mod file_logging {
        use super::*;

        #[test]
        fn should_default_to_hybrid_rotation_and_stdout() {
            let s = TelemetrySection::default();
            assert!(s.stdout, "默认保留控制台输出");
            assert_eq!(s.file.rotation, FileRotation::Mixed);
            assert_eq!(s.file.mode, FileWriteMode::Nonblocking);
            assert_eq!(s.file.overflow, FileOverflow::Drop);
            assert!(s.file.compress);
            assert_eq!(s.file.interval, Duration::from_secs(24 * 60 * 60));
        }

        #[test]
        fn should_reject_file_dir_problems() {
            let mut s = TelemetrySection::default();
            s.file.enabled = true;
            for bad in ["", "   ", "../logs", "logs/../escape", "lo\0gs"] {
                s.file.dir = bad.to_string();
                assert!(s.validate().is_err(), "应拒绝 file.dir = {bad:?}");
            }
            s.file.dir = "logs".to_string();
            assert!(s.validate().is_ok());
        }

        #[test]
        fn should_reject_path_separator_in_file_name() {
            let mut s = TelemetrySection::default();
            s.file.enabled = true;

            s.file.name = "app/sub".to_string();
            assert!(s.validate().is_err());
            s.file.name = String::new();
            assert!(s.validate().is_err());
            s.file.name = "app".to_string();

            s.file.suffix = "log\\gz".to_string();
            assert!(s.validate().is_err());
            s.file.suffix = "log".to_string();
            assert!(s.validate().is_ok());
        }

        #[test]
        fn should_require_size_and_interval_for_rotation() {
            let mut s = TelemetrySection::default();
            s.file.enabled = true;

            // size：max_size = 0 无法切割
            s.file.rotation = FileRotation::Size;
            s.file.max_size = Byte::from_u64(0);
            assert!(s.validate().is_err());

            // time：interval < 1s 视为配置错误（0 会被 rotate-rs 归一化成 24h）
            s.file.rotation = FileRotation::Time;
            s.file.interval = Duration::ZERO;
            assert!(s.validate().is_err());

            s.file.interval = Duration::from_secs(3600);
            s.file.max_size = Byte::from_u64(0);
            assert!(s.validate().is_ok(), "time 策略不要求 max_size");

            s.file.rotation = FileRotation::Mixed;
            assert!(s.validate().is_err(), "mixed 两个分量都要给");
        }

        #[test]
        fn should_reject_unbounded_backups_in_production() {
            let mut s = TelemetrySection::default();
            s.file.enabled = true;
            s.file.max_files = 0;
            // 非生产：只告警
            assert!(s.validate_for_env(false).is_ok());
            let error = s.validate_for_env(true).unwrap_err();
            assert!(error.contains("max_files"), "实际：{error}");

            // 上限防护
            s.file.max_files = u32::MAX;
            assert!(s.validate_for_env(false).is_err());
        }

        #[test]
        fn should_parse_file_section_keys() {
            let json = r#"
                {
                    "level": "debug",
                    "stdout": false,
                    "file": {
                        "enabled": true,
                        "dir": "/var/log/aurora",
                        "name": "api",
                        "suffix": ".log",
                        "rotation": "hybrid",
                        "max_size": "50MB",
                        "interval": "12h",
                        "max_files": 0,
                        "compress": false,
                        "mode": "non-blocking",
                        "channel_capacity": 4096,
                        "max_pending_bytes": "64MB",
                        "overflow": "block"
                    }
                }
            "#;
            let s: TelemetrySection = serde_json::from_str(json).unwrap();
            assert!(!s.stdout);
            assert!(s.file.enabled);
            assert_eq!(s.file.name, "api");
            assert_eq!(s.file.rotation, FileRotation::Mixed);
            assert_eq!(s.file.max_size.as_u64(), 50 * 1000 * 1000);
            assert_eq!(s.file.interval, Duration::from_secs(12 * 60 * 60));
            assert_eq!(s.file.max_files, 0);
            assert!(!s.file.compress);
            assert_eq!(s.file.mode, FileWriteMode::Nonblocking);
            assert_eq!(s.file.channel_capacity, 4096);
            assert_eq!(s.file.max_pending_bytes.as_u64(), 64 * 1000 * 1000);
            assert_eq!(s.file.overflow, FileOverflow::Block);
        }
    }

    #[test]
    fn should_validate_otel_sample_ratio() {
        let mut s = TelemetrySection::default();
        s.otel.enabled = true;
        s.otel.endpoint = Some("http://localhost:4317".to_string());

        s.otel.sample_ratio = 0.25;
        assert!(s.validate().is_ok());

        for bad in [-0.1, 1.5, f64::NAN] {
            s.otel.sample_ratio = bad;
            assert!(s.validate().is_err(), "应拒绝 sample_ratio = {bad}");
        }
    }

    #[test]
    fn should_reject_plaintext_otlp_endpoint_off_loopback() {
        let mut s = TelemetrySection::default();
        s.otel.enabled = true;

        // loopback 的明文端点可以（本地 collector）
        for ok in [
            "http://localhost:4317",
            "http://127.0.0.1:4317",
            "http://[::1]:4317",
            "https://otel.example.com:4317",
        ] {
            s.otel.endpoint = Some(ok.to_string());
            assert!(s.validate().is_ok(), "应接受 {ok}");
        }

        // 跨网段明文：日志/span 会以明文出网
        s.otel.endpoint = Some("http://collector.internal:4317".to_string());
        let error = s.validate().unwrap_err();
        assert!(error.contains("allow_insecure"), "实际：{error}");

        // 显式确认后放行
        s.otel.allow_insecure = true;
        assert!(s.validate().is_ok());

        // 非 http/https 直接拒绝
        s.otel.endpoint = Some("grpc://collector.internal:4317".to_string());
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_forbid_off_level_in_production() {
        let mut s = TelemetrySection {
            level: "off".to_string(),
            ..TelemetrySection::default()
        };
        assert!(s.validate_for_env(false).is_ok(), "非生产允许");
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("off"), "实际：{error}");

        s.level = "aurora=debug,tower_http=warn".to_string();
        assert!(s.validate_for_env(true).is_ok());
    }

    #[test]
    fn should_detect_trace_level_in_expression() {
        assert!(level_has_trace("trace"));
        assert!(level_has_trace("aurora=trace,info"));
        assert!(!level_has_trace("aurora=debug,info"));
        // target 名里带 trace 不算
        assert!(!level_has_trace("my_trace_layer=debug"));
    }

    #[test]
    fn should_parse_nested_file_and_metrics() {
        let json = r#"
            {
                "level": "debug",
                "format": "pretty",
                "file": {
                    "enabled": true,
                    "max_size": "50MB",
                    "rotation": "time",
                    "interval": "6h",
                    "compress": false
                },
                "metrics": { "enabled": true, "path": "/m" }
            }
        "#;
        let s: TelemetrySection = serde_json::from_str(json).unwrap();
        assert_eq!(s.level, "debug");
        assert_eq!(s.format, LogFormat::Pretty);
        assert!(s.file.enabled);
        assert_eq!(s.file.max_size.as_u64(), 50 * 1000 * 1000);
        assert_eq!(s.file.rotation, FileRotation::Time);
        assert!(s.metrics.enabled);
        assert_eq!(s.metrics.path, "/m");
        assert!(!s.otel.enabled);
    }
}
