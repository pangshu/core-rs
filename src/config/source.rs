//! 配置来源链：`default.toml → {env}.toml → 环境变量 →（可选）配置中心`，
//! 靠后者覆盖前者（文档 三·4）。
//!
//! - 环境变量前缀 `APP_`，`__` 表示层级：`APP_SERVER__PORT=9090` 覆盖 `server.port`；
//! - 敏感项（DB 密码、JWT secret）只走环境变量，不写入 toml；
//! - 配置中心（Nacos / Apollo / etcd / Consul）是可选来源，feature = "config-remote"。

use serde::de::DeserializeOwned; // 引入可反序列化 trait 作为加载泛型约束

use super::env::Environment; // 引入运行环境类型，决定加载哪份 env toml

/// 加载选项
#[derive(Debug, Clone)] // 派生调试与克隆能力
pub struct LoadOptions { // 配置加载选项结构
    /// 配置目录，默认 `config`
    pub dir: String, // 配置目录路径
    pub environment: Environment, // 运行环境，决定加载的 env toml
    /// 是否叠加环境变量覆盖（测试场景可关）
    pub env_vars: bool, // 是否启用环境变量来源
    /// 配置中心轮询地址（feature = "config-remote"）；空 = 不启用
    #[cfg(feature = "config-remote")] // 仅在开启远端配置 feature 时编译
    pub remote_url: Option<String>, // 配置中心地址，None 表示不启用
    /// 预取好的远端配置文本（feature = "config-remote"）：bootstrap 在 async
    /// 上下文先行拉取后注入，同步构建链只消费文本，不再发起网络请求
    #[cfg(feature = "config-remote")] // 仅在开启远端配置 feature 时编译
    pub(crate) remote_text: Option<String>, // 预取的远端 TOML 文本，供同步链消费
}

impl LoadOptions { // 为加载选项实现构造与链式配置
    pub fn new(environment: Environment) -> Self { // 用指定环境构造默认选项
        Self { // 构造加载选项
            dir: "config".to_string(), // 默认配置目录为 config
            environment, // 保存传入的运行环境
            env_vars: true, // 默认启用环境变量覆盖
            #[cfg(feature = "config-remote")] // 仅在开启远端配置 feature 时编译
            remote_url: std::env::var("APP_CONFIG_REMOTE_URL").ok().filter(|s| !s.is_empty()), // 从环境变量读取配置中心地址，空串视为未配置
            #[cfg(feature = "config-remote")] // 仅在开启远端配置 feature 时编译
            remote_text: None, // 远端文本默认未预取
        }
    }

    pub fn dir(mut self, dir: impl Into<String>) -> Self { // 链式设置配置目录
        self.dir = dir.into(); // 更新配置目录
        self // 返回自身以支持链式调用
    }
}

/// 组装配置源并反序列化为目标结构。文件全部可选（不存在时跳过），
/// 全部缺失且无环境变量时取类型默认值。
pub fn load<T: DeserializeOwned>(opts: &LoadOptions) -> Result<T, config::ConfigError> { // 按来源链加载并反序列化配置
    build_config(opts)?.try_deserialize() // 先构建合并配置再反序列化为目标类型
}

/// 便捷加载：`load::<Settings>(&LoadOptions::new(env))` 的泛型入口
pub fn load_with<T: DeserializeOwned>(opts: &LoadOptions) -> Result<T, config::ConfigError> { // 泛型便捷加载函数，返回目标配置类型
    load(opts) // 委托给 load 完成来源组装与反序列化
}

pub(crate) fn build_config(opts: &LoadOptions) -> Result<config::Config, config::ConfigError> { // 按优先级组装各配置来源
    let mut builder = config::Config::builder(); // 创建配置构建器

    // 1. default.toml
    builder = builder.add_source( // 追加最低优先级的默认配置文件来源
        config::File::with_name(&format!("{}/default", opts.dir)).required(false), // 加载 {dir}/default.toml，不存在则跳过
    );
    // 2. {env}.toml
    builder = builder.add_source( // 追加环境配置文件来源，覆盖默认
        config::File::with_name(&format!("{}/{}", opts.dir, opts.environment.file_stem())) // 加载 {dir}/{env}.toml
            .required(false), // 文件不存在时不报错
    );
    // 3. 配置中心（可选来源，靠后覆盖）：文本由 bootstrap 阶段 async 预取注入
    #[cfg(feature = "config-remote")] // 仅在开启远端配置 feature 时编译
    if let Some(text) = &opts.remote_text { // 若已预取到远端配置文本
        // 远端返回 TOML 文本；解析失败视为配置错误（fail-fast）
        builder = builder.add_source(config::File::from_str( // 追加远端 TOML 文本来源，覆盖本地
            text, // 预取的远端配置文本
            config::FileFormat::Toml, // 指定按 TOML 格式解析
        ));
    }
    // 4. 环境变量（优先级最高）
    if opts.env_vars { // 若启用环境变量来源
        builder = builder.add_source( // 追加优先级最高的环境变量来源
            // prefix_separator 必须显式指定，否则会跟随 separator("__")
            // 导致 APP_ 前缀匹配不上
            config::Environment::with_prefix("APP") // 以 APP 为环境变量前缀
                .prefix_separator("_") // 前缀与变量名间用单下划线分隔
                .separator("__") // 双下划线表示配置层级
                .try_parsing(true), // 尝试把字符串值解析为对应基础类型
        );
    }
    builder.build() // 构建并返回最终合并后的配置
}

/// async 拉取配置中心文本（带超时与一次重试）。
///
/// 必须在 bootstrap 的 async 阶段调用；不得在同步上下文用 `Handle::block_on`
/// 桥接——在 async 执行上下文里调用 block_on 会直接 panic。
#[cfg(feature = "config-remote")] // 仅在开启远端配置 feature 时编译
pub(crate) async fn fetch_remote_async( // 异步拉取配置中心文本
    url: &str, // 配置中心地址
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> { // 返回配置文本或错误
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10); // 单次请求超时 10 秒
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1); // 重试前等待 1 秒
    let mut last_err: Option<Box<dyn std::error::Error + Send + Sync>> = None; // 记录最近一次错误
    for attempt in 0..2 { // 最多尝试两次（首次 + 一次重试）
        if attempt > 0 { // 非首次尝试时先退避等待
            tokio::time::sleep(RETRY_DELAY).await; // 等待重试间隔
        }
        match tokio::time::timeout(TIMEOUT, reqwest::get(url)).await { // 带超时发起 GET 请求
            Ok(Ok(resp)) => match resp.error_for_status() { // 请求成功则校验 HTTP 状态码
                Ok(r) => match r.text().await { // 状态码正常则读取响应文本
                    Ok(text) => return Ok(text), // 成功读到文本，直接返回
                    Err(e) => last_err = Some(e.into()), // 读取文本失败，记录错误待重试
                },
                Err(e) => last_err = Some(e.into()), // 状态码异常，记录错误待重试
            },
            Ok(Err(e)) => last_err = Some(e.into()), // 请求本身失败，记录错误待重试
            Err(_) => { // 请求超时
                last_err = Some( // 记录超时错误
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "config fetch timeout") // 构造超时 IO 错误
                        .into(), // 转换为统一错误类型
                )
            }
        }
    }
    Err(last_err // 两次尝试均失败，返回最近一次错误
        .unwrap_or_else(|| std::io::Error::other("config fetch failed").into())) // 兜底构造一个失败错误
}
