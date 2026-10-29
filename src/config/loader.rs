//! 配置加载：**读哪些文件由应用层给，框架只负责读、合并、反序列化**。
//!
//! # 职责边界
//!
//! | 谁 | 管什么 |
//! |---|---|
//! | 应用层 | 读几份文件、文件名、目录、顺序、环境从哪个变量来 |
//! | 框架 | 读文件 → 分层合并 → 类型强制 → 反序列化 → `validate()` → 生产护栏 |
//!
//! 框架**不认识任何具体路径**：没有"默认 `config/` 目录"、没有"`{env}.toml` 命名约定"。
//! 这些是部署约定，每个应用可能不同（一份文件 / 四份 / 十份），写进基座就等于让基座
//! 只服务一种布局。
//!
//! 想要「按环境选文件」这套约定，用可选的 [`preset`](crate::config::preset) 模块：
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use aurora::config::{ConfigSource, Environment};
//!
//! // ① 完全自己决定
//! let config = ConfigSource::new(Environment::Production)
//!     .file("config/base.toml")
//!     .file("config/production.toml")
//!     .env_prefix("AURORA")
//!     .load()?;
//!
//! // ② 用可选约定（4 个环境名 → 4 份文件）
//! let config = aurora::config::preset::from_env_var("AURORA_ENV", "config")?.load()?;
//! # let _ = config;
//! # Ok(())
//! # }
//! ```
//!
//! # 为什么 `ConfigSource` 是个"值"
//!
//! 热更新必须重放**同一套来源**：如果 [`ConfigWatcher`](crate::config::ConfigWatcher) /
//! [`NacosWatcher`](crate::config::NacosWatcher) 各自再推导一遍"该读哪些文件"，就会出现
//! 两份真相——启动读 A 文件、热更新读 B 文件。所以 `ConfigSource` 是 `Clone` 的，
//! 启动与热更新共用同一个实例。
//!
//! # 优先级（低 → 高）
//!
//! 1. 代码默认值（各 section 的 `serde(default)`）
//! 2. [`ConfigSource::files`] 里**按声明顺序**逐个叠加（后者覆盖前者）
//! 3. 环境变量 `{prefix}_{SECTION}__{KEY}`
//! 4. 配置中心（`nacos`，插在 2 与 3 之间）
//!
//! # fail-closed
//!
//! 声明了的文件**必须存在**：缺文件直接报错，不静默跳过。否则"配置没搬过去"在生产环境
//! 会表现为"服务照跑、大部分值是代码默认值"——比启动失败危险得多。

use std::path::{Path, PathBuf};

use config::{Config, File, FileFormat};

use crate::boot::startup::StartupError;
use crate::config::AppConfig;
use crate::config::Environment;
use crate::config::environment;

/// 配置来源：**由应用层构造**，框架只消费。
///
/// 见模块文档。`Clone` 是刻意的——热更新要重放同一套来源。
#[derive(Debug, Clone)]
pub struct ConfigSource {
    files: Vec<PathBuf>,
    env: Environment,
    env_prefix: Option<String>,
}

impl ConfigSource {
    /// 以指定环境构造空来源（环境决定生产护栏是否生效）。
    ///
    /// 默认带 `AURORA` 前缀的环境变量叠加层，可用 [`Self::without_env_overlay`] 关掉。
    #[must_use]
    pub fn new(env: Environment) -> Self {
        Self {
            files: Vec::new(),
            env,
            env_prefix: Some(environment::DEFAULT_ENV_PREFIX.to_string()),
        }
    }

    /// 追加一个文件（**顺序即优先级，后加者覆盖前者**）。
    #[must_use]
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.files.push(path.into());
        self
    }

    /// 批量追加文件（保持迭代顺序）。
    #[must_use]
    pub fn files<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.files.extend(paths.into_iter().map(Into::into));
        self
    }

    /// 指定环境变量前缀（`{prefix}_{SECTION}__{KEY}`）。
    #[must_use]
    pub fn env_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.env_prefix = Some(prefix.into());
        self
    }

    /// 关闭环境变量叠加层（只读文件）。
    ///
    /// 慎用：部署侧就失去了"不改文件临时覆盖"的能力（排障时最常用的手段）。
    #[must_use]
    pub fn without_env_overlay(mut self) -> Self {
        self.env_prefix = None;
        self
    }

    /// 运行环境。
    #[must_use]
    pub fn env(&self) -> Environment {
        self.env
    }

    /// 声明过的文件（按优先级从低到高）。
    #[must_use]
    pub fn file_paths(&self) -> &[PathBuf] {
        &self.files
    }

    /// 读 + 合并 + 反序列化，**不校验**。
    ///
    /// 用于"先看配置、再决定怎么校验"的场合（如先读 `[nacos].enabled`）。
    /// 绝大多数场景应该用 [`Self::load`]。
    ///
    /// # Errors
    /// 声明的文件不存在、扩展名不受支持、内容解析失败或字段类型不符时返回
    /// [`StartupError::Config`]。
    pub fn parse(&self) -> Result<AppConfig, StartupError> {
        let missing: Vec<String> = self
            .files
            .iter()
            .filter(|path| !path.is_file())
            .map(|path| path.display().to_string())
            .collect();
        if !missing.is_empty() {
            return Err(StartupError::Config(format!(
                "配置文件不存在：{}。应用层声明的文件必须存在——\
                 静默跳过会让服务带着代码默认值跑起来，在生产环境比启动失败更危险",
                missing.join(", ")
            )));
        }

        let mut builder = Config::builder();
        for path in &self.files {
            let path_str = path.to_string_lossy().into_owned();
            let format = format_from_path(&path_str)?;
            builder = builder.add_source(File::new(&path_str, format));
        }
        if let Some(prefix) = &self.env_prefix {
            builder = builder.add_source(environment::env_source(prefix));
        }

        let mut config = builder
            .build()
            .and_then(Config::try_deserialize::<AppConfig>)
            .map_err(|error| StartupError::Config(error.to_string()))?;
        // 环境不来自文件，由构造者给：读写两侧共用同一个值，不存在"读 A 文件、按 B 环境校验"
        config.env = self.env;
        Ok(config)
    }

    /// 读 + 合并 + 反序列化 + **校验**（启动路径用这个）。
    ///
    /// # Errors
    /// 同 [`Self::parse`]，外加任一 section 的语义校验失败。
    pub fn load(&self) -> Result<AppConfig, StartupError> {
        let config = self.parse()?;
        config.validate()?;
        Ok(config)
    }

    /// 读 + 合并 + 反序列化 + **叠加配置中心** + 校验。
    ///
    /// `[nacos].enabled = false` 时等价于 [`Self::load`]。
    ///
    /// # Errors
    /// 本地部分同 [`Self::load`]；远端不可达或内容非法时返回 [`StartupError::Config`]。
    #[cfg(feature = "nacos")]
    pub async fn load_with_remote(&self) -> Result<AppConfig, StartupError> {
        let local = self.parse()?;
        if !local.nacos.enabled {
            local.validate()?;
            return Ok(local);
        }

        let client = crate::config::NacosClient::new(&local.nacos)
            .map_err(|error| StartupError::Config(format!("Nacos 客户端初始化失败：{error}")))?;
        let content = client
            .fetch()
            .await
            .map_err(|error| StartupError::Config(format!("从 Nacos 拉取配置失败：{error}")))?;
        let config = self.parse_with_remote(&content, local.nacos.format.to_file_format())?;
        config.validate()?;
        Ok(config)
    }

    /// 在本地来源之上叠加一段**远端配置原文**（配置中心热更新用）。
    ///
    /// # Errors
    /// 远端内容不在白名单内、解析失败或字段类型不符时返回 [`StartupError::Config`]。
    #[cfg(feature = "nacos")]
    pub fn parse_with_remote(
        &self,
        content: &str,
        format: FileFormat,
    ) -> Result<AppConfig, StartupError> {
        deny_remote_sections(content, format)?;

        let mut builder = Config::builder();
        for path in &self.files {
            let path_str = path.to_string_lossy().into_owned();
            let local_format = format_from_path(&path_str)?;
            builder = builder.add_source(File::new(&path_str, local_format));
        }
        builder = builder.add_source(File::from_str(content, format));
        if let Some(prefix) = &self.env_prefix {
            builder = builder.add_source(environment::env_source(prefix));
        }

        let mut config = builder
            .build()
            .and_then(Config::try_deserialize::<AppConfig>)
            .map_err(|error| StartupError::Config(error.to_string()))?;
        config.env = self.env;
        Ok(config)
    }
}

/// 配置中心**禁止下发**的 section。
///
/// 这些段要么定义「信任根」（TLS 证书、JWT 密钥、加密密钥、RBAC 策略、配置中心
/// 自身地址），要么定义「信任判定」（谁算可信代理、谁被 IP 规则拦截）与
/// 「运行身份」（`[app]` 的 `allow_insecure` 绊线）。
/// 允许远端覆盖等于把安全边界交给配置中心：一旦它被攻破或通道被中间人劫持，
/// 攻击者可以换掉 JWT 密钥（自己签发 token）、关掉 TLS，或者改掉可信代理网段
/// 从而伪造 `X-Forwarded-For`。
///
/// 它们只允许由**本地文件 / 环境变量**提供。
#[cfg(feature = "nacos")]
pub(crate) const REMOTE_DENIED_SECTIONS: [&str; 10] = [
    "app", "server", "auth", "authz", "security", "tls", "nacos", "database", "cache", "queue",
];

/// 配置中心**允许下发**的键（精确路径，`a.b.c` 形式）。
///
/// 白名单制而非黑名单制：新增配置段时默认「不可远程下发」（fail-closed），
/// 想放开必须在这里显式登记，评审时一眼可见。
///
/// 刻意**不**收录的敏感键（举例）：
/// - `telemetry.metrics.token` / `telemetry.metrics.enabled`：抓取令牌与端点开关，
///   远端能改等于让指标端点裸奔；
/// - `http.ip_filter.*` / `http.csrf.*`：能关掉防护；
/// - `server.trust_proxy` / `server.trusted_proxies`：能改信任边界。
#[cfg(feature = "nacos")]
pub(crate) const REMOTE_ALLOWED_KEYS: [&str; 6] = [
    "telemetry.level",
    "telemetry.otel.sample_ratio",
    "telemetry.otel.logs",
    "http.access_log.enabled",
    "http.compression.enabled",
    "ratelimit.enabled",
];

/// 拒绝配置中心下发的不受信任配置（见 [`REMOTE_DENIED_SECTIONS`] /
/// [`REMOTE_ALLOWED_KEYS`]）。
///
/// 判定顺序（两级，都在**解析后**按叶子路径做）：
/// 1. 命中禁止段 → 报「受保护的 section」（给出更贴切的错误文案）；
/// 2. 未登记在白名单 → 报「不在白名单内」（覆盖未来新增的段，默认拒绝）。
///
/// 只看**叶子路径**而不是顶层键：`[server]` 被禁止时，`server.port` 与
/// `server.trust_proxy` 都要拦；而白名单必须精确到 `telemetry.level`，
/// 否则 `telemetry.metrics.token` 会顺着 `telemetry` 这个前缀漏过去。
#[cfg(feature = "nacos")]
fn deny_remote_sections(content: &str, format: FileFormat) -> Result<(), StartupError> {
    let remote = Config::builder()
        .add_source(File::from_str(content, format))
        .build()
        .map_err(|e| StartupError::Config(format!("配置中心内容解析失败：{e}")))?;
    let value: serde_json::Value = remote
        .try_deserialize()
        .map_err(|e| StartupError::Config(format!("配置中心内容结构非法：{e}")))?;

    let mut paths = Vec::new();
    leaf_paths(&value, "", &mut paths);

    let denied: Vec<&str> = paths
        .iter()
        .filter(|path| {
            path.split('.')
                .next()
                .is_some_and(|section| REMOTE_DENIED_SECTIONS.contains(&section))
        })
        .map(String::as_str)
        .collect();
    if !denied.is_empty() {
        return Err(StartupError::Config(format!(
            "配置中心下发了受保护的键 {denied:?}：这些段定义信任根 / 信任判定，\
             只允许由本地文件 / 环境变量提供（防止配置中心被攻破后替换 JWT 密钥、\
             关闭 TLS，或改掉可信代理网段伪造 X-Forwarded-For）"
        )));
    }

    let unknown: Vec<&str> = paths
        .iter()
        .filter(|path| !REMOTE_ALLOWED_KEYS.contains(&path.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(StartupError::Config(format!(
            "配置中心下发了未登记的键 {unknown:?}：远端配置采用白名单制，\
             新增可下发项必须在 config::loader::REMOTE_ALLOWED_KEYS 显式登记"
        )));
    }
    Ok(())
}

/// 把 JSON 值展开成叶子路径（`a.b.c`）。
///
/// - 对象：递归下钻；
/// - 数组 / 标量：整体视为一个叶子（数组内部元素不参与白名单比对，
///   因为数组型配置几乎都是"一组规则"，应整体决定是否允许下发）；
/// - 空对象：记为它自己的路径（否则 `[a]` 这种空表会被静默放过）。
#[cfg(feature = "nacos")]
fn leaf_paths(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                leaf_paths(child, &path, out);
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
        }
    }
}

/// 在同一目录下按 `.toml` > `.yaml`/`.yml` > `.json` 查找第一个存在的文件。
///
/// 供 [`preset`](crate::config::preset) 使用——框架自身不调用它。
pub(crate) fn find_file(dir: &Path, stem: &str) -> Option<(PathBuf, FileFormat)> {
    const CANDIDATES: [(&str, FileFormat); 4] = [
        ("toml", FileFormat::Toml),
        ("yaml", FileFormat::Yaml),
        ("yml", FileFormat::Yaml),
        ("json", FileFormat::Json),
    ];
    for (ext, format) in CANDIDATES {
        let path = dir.join(format!("{stem}.{ext}"));
        if path.is_file() {
            return Some((path, format));
        }
    }
    None
}

/// 由文件扩展名推断格式。
pub(crate) fn format_from_path(path: &str) -> Result<FileFormat, StartupError> {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("toml") => Ok(FileFormat::Toml),
        Some("yaml" | "yml") => Ok(FileFormat::Yaml),
        Some("json") => Ok(FileFormat::Json),
        other => Err(StartupError::Config(format!(
            "unsupported config file extension: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurora-source-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 框架不认识任何路径：给几个文件就读几个，顺序即优先级。
    #[test]
    fn should_merge_files_in_declared_order() {
        let dir = temp_dir("order");
        let base = dir.join("base.toml");
        let over = dir.join("over.toml");
        std::fs::write(&base, "[server]\nport = 7000\nhost = \"127.0.0.1\"\n").unwrap();
        std::fs::write(&over, "[server]\nport = 8000\n").unwrap();

        let config = ConfigSource::new(Environment::Development)
            .file(&base)
            .file(&over)
            .parse()
            .expect("应加载成功");
        assert_eq!(config.server.port, 8000, "后声明的文件覆盖先声明的");
        assert_eq!(config.server.host, "127.0.0.1", "未覆盖项保留");
        assert_eq!(config.env, Environment::Development);
    }

    /// 声明的文件必须存在——不静默跳过。
    #[test]
    fn should_reject_missing_declared_file() {
        let dir = temp_dir("missing");
        let exists = dir.join("a.toml");
        std::fs::write(&exists, "[server]\nport = 7000\n").unwrap();
        let absent = dir.join("nope.toml");

        let error = ConfigSource::new(Environment::Development)
            .file(&exists)
            .file(&absent)
            .parse()
            .expect_err("缺文件必须报错");
        assert!(
            error.to_string().contains("配置文件不存在"),
            "实际：{error}"
        );
    }

    /// 环境来自构造者，与"读哪些文件"完全解耦。
    #[test]
    fn should_take_env_from_source_not_from_file() {
        let dir = temp_dir("env");
        let file = dir.join("whatever-name.toml");
        std::fs::write(&file, "[app]\nname = \"x\"\n").unwrap();

        let dev = ConfigSource::new(Environment::Development)
            .file(&file)
            .parse()
            .unwrap();
        assert!(!dev.is_production());

        let prod = ConfigSource::new(Environment::Production)
            .file(&file)
            .parse()
            .unwrap();
        assert!(prod.is_production(), "同一份文件，环境由构造者决定");
    }

    /// 零文件是合法输入（配置全来自环境变量 / 代码默认值）。
    #[test]
    fn should_allow_empty_file_list() {
        let config = ConfigSource::new(Environment::Development)
            .without_env_overlay()
            .parse()
            .expect("零文件应合法");
        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn should_expose_declared_files() {
        let source = ConfigSource::new(Environment::Test)
            .file("a.toml")
            .file("b.yaml");
        assert_eq!(source.env(), Environment::Test);
        assert_eq!(
            source.file_paths(),
            &[PathBuf::from("a.toml"), PathBuf::from("b.yaml")]
        );
    }
}

#[cfg(all(test, feature = "nacos"))]
mod nacos_tests {
    use super::*;

    #[test]
    fn should_reject_protected_sections_from_remote() {
        // 受保护段（定义信任根 / 信任判定）不允许由配置中心下发
        for content in [
            "[tls]\nenabled = true\n",
            "[auth]\n[auth.jwt]\nsecret = \"x\"\n",
            "[security]\n[security.hmac]\nkey = \"x\"\n",
            "[nacos]\nserver_addr = \"http://evil\"\n",
            "[authz]\npolicy_path = \"x\"\n",
            // `[app]` 含 allow_insecure 绊线，一并拦住
            "[app]\nallow_insecure = true\n",
            "[server]\ntrust_proxy = true\ntrusted_proxies = [\"10.0.0.0/8\"]\n",
            "[database]\nurl = \"postgres://evil/db\"\n",
        ] {
            let error =
                deny_remote_sections(content, FileFormat::Toml).expect_err("受保护段必须被拒绝");
            assert!(error.to_string().contains("受保护"), "实际：{error}");
        }
    }

    #[test]
    fn should_allow_whitelisted_keys_from_remote() {
        let content = "[telemetry]\nlevel = \"debug\"\n\n[telemetry.otel]\nsample_ratio = 0.1\n";
        assert!(deny_remote_sections(content, FileFormat::Toml).is_ok());
    }

    #[test]
    fn should_reject_unlisted_keys_from_remote() {
        // 白名单制：没登记过的键一律拒绝（将来新增配置段默认安全）
        for content in [
            "[app]\nname = \"remote\"\n",                          // 段被禁
            "[server]\nport = 7002\n",                             // 段被禁
            "[telemetry]\nlevel = \"debug\"\nformat = \"json\"\n", // format 未登记
            "[telemetry.metrics]\ntoken = \"\"\n",                 // 敏感键，刻意未登记
            "[http]\ncsrf = { enabled = false }\n",                // 能关掉防护，刻意未登记
        ] {
            let error =
                deny_remote_sections(content, FileFormat::Toml).expect_err("未登记键必须被拒绝");
            assert!(
                error.to_string().contains("未登记") || error.to_string().contains("受保护"),
                "实际：{error}"
            );
        }
    }

    #[test]
    fn should_expand_nested_paths_to_leaves() {
        let mut out = Vec::new();
        let value = serde_json::json!({
            "telemetry": { "level": "debug", "otel": { "sample_ratio": 0.1 } },
            "http": { "compression": { "enabled": false } },
            "empty": {}
        });
        leaf_paths(&value, "", &mut out);
        out.sort();
        assert_eq!(
            out,
            vec![
                "empty",
                "http.compression.enabled",
                "telemetry.level",
                "telemetry.otel.sample_ratio",
            ]
        );
    }
}
