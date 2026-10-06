//! 配置来源链：`default.toml → {env}.toml → 环境变量 →（可选）配置中心`，
//! 靠后者覆盖前者（文档 三·4）。
//!
//! - 环境变量前缀 `APP_`，`__` 表示层级：`APP_SERVER__PORT=9090` 覆盖 `server.port`；
//! - 敏感项（DB 密码、JWT secret）只走环境变量，不写入 toml；
//! - 配置中心（Nacos / Apollo / etcd / Consul）是可选来源，feature = "config-remote"。

use serde::de::DeserializeOwned;

use super::env::Environment;

/// 加载选项
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// 配置目录，默认 `config`
    pub dir: String,
    pub environment: Environment,
    /// 是否叠加环境变量覆盖（测试场景可关）
    pub env_vars: bool,
    /// 配置中心轮询地址（feature = "config-remote"）；空 = 不启用
    #[cfg(feature = "config-remote")]
    pub remote_url: Option<String>,
    /// 预取好的远端配置文本（feature = "config-remote"）：bootstrap 在 async
    /// 上下文先行拉取后注入，同步构建链只消费文本，不再发起网络请求
    #[cfg(feature = "config-remote")]
    pub(crate) remote_text: Option<String>,
}

impl LoadOptions {
    pub fn new(environment: Environment) -> Self {
        Self {
            dir: "config".to_string(),
            environment,
            env_vars: true,
            #[cfg(feature = "config-remote")]
            remote_url: std::env::var("APP_CONFIG_REMOTE_URL").ok().filter(|s| !s.is_empty()),
            #[cfg(feature = "config-remote")]
            remote_text: None,
        }
    }

    pub fn dir(mut self, dir: impl Into<String>) -> Self {
        self.dir = dir.into();
        self
    }
}

/// 组装配置源并反序列化为目标结构。文件全部可选（不存在时跳过），
/// 全部缺失且无环境变量时取类型默认值。
pub fn load<T: DeserializeOwned>(opts: &LoadOptions) -> Result<T, config::ConfigError> {
    build_config(opts)?.try_deserialize()
}

pub(crate) fn build_config(opts: &LoadOptions) -> Result<config::Config, config::ConfigError> {
    let mut builder = config::Config::builder();

    // 1. default.toml
    builder = builder.add_source(
        config::File::with_name(&format!("{}/default", opts.dir)).required(false),
    );
    // 2. {env}.toml
    builder = builder.add_source(
        config::File::with_name(&format!("{}/{}", opts.dir, opts.environment.file_stem()))
            .required(false),
    );
    // 3. 配置中心（可选来源，靠后覆盖）：文本由 bootstrap 阶段 async 预取注入
    #[cfg(feature = "config-remote")]
    if let Some(text) = &opts.remote_text {
        // 远端返回 TOML 文本；解析失败视为配置错误（fail-fast）
        builder = builder.add_source(config::File::from_str(
            text,
            config::FileFormat::Toml,
        ));
    }
    // 4. 环境变量（优先级最高）
    if opts.env_vars {
        builder = builder.add_source(
            // prefix_separator 必须显式指定，否则会跟随 separator("__")
            // 导致 APP_ 前缀匹配不上
            config::Environment::with_prefix("APP")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        );
    }
    builder.build()
}

/// async 拉取配置中心文本（带超时与一次重试）。
///
/// 必须在 bootstrap 的 async 阶段调用；不得在同步上下文用 `Handle::block_on`
/// 桥接——在 async 执行上下文里调用 block_on 会直接 panic。
#[cfg(feature = "config-remote")]
pub(crate) async fn fetch_remote_async(
    url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
    let mut last_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(RETRY_DELAY).await;
        }
        match tokio::time::timeout(TIMEOUT, reqwest::get(url)).await {
            Ok(Ok(resp)) => match resp.error_for_status() {
                Ok(r) => match r.text().await {
                    Ok(text) => return Ok(text),
                    Err(e) => last_err = Some(e.into()),
                },
                Err(e) => last_err = Some(e.into()),
            },
            Ok(Err(e)) => last_err = Some(e.into()),
            Err(_) => {
                last_err = Some(
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "config fetch timeout")
                        .into(),
                )
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| std::io::Error::other("config fetch failed").into()))
}
