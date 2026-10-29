//! Nacos 配置中心（feature `nacos`）。
//!
//! 走 Nacos 的 **HTTP OpenAPI**（`/nacos/v1/cs/configs`），不引入 gRPC 客户端：
//! 只需要「拉一段配置文本」这一件事，HTTP 足够且依赖最轻。
//!
//! ## 优先级
//!
//! 默认值 → 本地文件（`ConfigSource` 声明的那些）→ **Nacos** → 环境变量
//!
//! 环境变量放最后：运维排障时可以不碰配置中心临时覆盖（如调日志级别）。
//! 本地文件由 [`ConfigSource`] 决定——框架不认识具体路径。
//!
//! ## 热更新
//!
//! [`NacosWatcher`] 按 `[nacos].poll_interval` **轮询**，内容变化才重新解析并原子替换
//! [`ConfigHandle`]。未实现 Nacos 的长轮询（`/cs/configs/listener` + `Long-Pulling-Timeout`），
//! 因此变更生效延迟上限 = 一个轮询周期（默认 30s）。
//!
//! 生效范围与本地文件热更新一致：**只有请求处理时重新 `state.config.load()` 的值即时生效**，
//! 中间件栈 / 监听地址 / 连接池仍需重启。

use std::sync::Mutex;

use crate::boot::startup::StartupError;
use crate::config::sections::NacosSection;
use crate::config::{AppConfig, ConfigHandle, ConfigSource};
use crate::errors::{AppError, AppResult, ErrorCode};

/// Nacos HTTP 客户端。
#[derive(Clone)]
pub struct NacosClient {
    http: reqwest::Client,
    section: NacosSection,
    /// 鉴权 token 缓存（开启鉴权时才有值）。
    token: std::sync::Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for NacosClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NacosClient")
            .field("server_addr", &self.section.server_addr)
            .field("data_id", &self.section.data_id)
            .field("group", &self.section.group)
            .finish_non_exhaustive()
    }
}

impl NacosClient {
    /// 创建客户端。
    ///
    /// # Errors
    /// HTTP 客户端构建失败时返回 `50005`。
    pub fn new(section: &NacosSection) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(section.timeout)
            .build()
            .map_err(|error| {
                // 构建失败信息可能含代理 / TLS 配置细节，只进日志
                tracing::error!(%error, "Nacos HTTP 客户端创建失败");
                AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    ErrorCode::SERVICE_UNAVAILABLE.default_message(),
                )
            })?;
        Ok(Self {
            http,
            section: section.clone(),
            token: std::sync::Arc::new(Mutex::new(None)),
        })
    }

    /// 服务地址（去掉尾部 `/`）。
    fn base(&self) -> &str {
        self.section.server_addr.trim_end_matches('/')
    }

    /// 取 accessToken（未配置用户名时返回 `None`）。
    async fn access_token(&self) -> AppResult<Option<String>> {
        if self.section.username.trim().is_empty() {
            return Ok(None);
        }
        if let Some(token) = self.token.lock().ok().and_then(|slot| slot.clone()) {
            return Ok(Some(token));
        }

        let url = format!("{}/nacos/v1/auth/login", self.base());
        let response = self
            .http
            .post(&url)
            .form(&[
                ("username", self.section.username.as_str()),
                ("password", self.section.password.as_str()),
            ])
            .send()
            .await
            .map_err(net_err)?;

        if !response.status().is_success() {
            // 对外固定文案：状态码与内网地址只进日志
            tracing::warn!(status = %response.status(), "Nacos 登录失败");
            return Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                ErrorCode::UPSTREAM_ERROR.default_message(),
            ));
        }

        let body: serde_json::Value = response.json().await.map_err(net_err)?;
        let token = body
            .get("accessToken")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                AppError::biz(ErrorCode::UPSTREAM_ERROR, "Nacos 登录响应缺少 accessToken")
            })?;

        if let Ok(mut slot) = self.token.lock() {
            *slot = Some(token.clone());
        }
        Ok(Some(token))
    }

    /// 拉取配置原文。
    ///
    /// accessToken 过期 / 被吊销（401 / 403）时会清缓存重登一次；响应体有大小
    /// 上限，避免异常配置中心把进程内存打满。
    ///
    /// # Errors
    /// 网络失败、鉴权失败或服务端返回非 2xx 时返回 `50003`。
    pub async fn fetch(&self) -> AppResult<String> {
        let url = format!("{}/nacos/v1/cs/configs", self.base());
        // `retried` 保证「重登」最多发生一次，不会死循环
        let mut retried = false;
        loop {
            let token = self.access_token().await?;
            let mut request = self.http.get(&url).query(&self.section.config_query());
            if let Some(token) = token.as_deref() {
                request = request.query(&[("accessToken", token)]);
            }

            let response = request.send().await.map_err(net_err)?;
            let status = response.status();

            if matches!(status.as_u16(), 401 | 403) && token.is_some() && !retried {
                tracing::warn!("Nacos 鉴权失败（accessToken 可能已过期），清缓存后重试一次");
                self.clear_token();
                retried = true;
                continue;
            }
            if !status.is_success() {
                // dataId / group 属于内部配置标识，不对外返回
                tracing::warn!(
                    %status,
                    data_id = %self.section.data_id,
                    group = %self.section.group,
                    "拉取 Nacos 配置失败"
                );
                return Err(AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    ErrorCode::UPSTREAM_ERROR.default_message(),
                ));
            }
            return read_limited(response, MAX_REMOTE_CONFIG_BYTES).await;
        }
    }

    /// 清空 accessToken 缓存（token 过期 / 被吊销时）。
    fn clear_token(&self) {
        if let Ok(mut slot) = self.token.lock() {
            *slot = None;
        }
    }

    /// 拉取远端内容，叠加到 `source` 声明的本地文件之上，并做启动期校验。
    ///
    /// # Errors
    /// 拉取失败、内容不在白名单内、解析失败或校验不通过时返回 [`StartupError`]。
    pub async fn load_config(&self, source: &ConfigSource) -> Result<AppConfig, StartupError> {
        let content = self
            .fetch()
            .await
            .map_err(|error| StartupError::Config(format!("从 Nacos 拉取配置失败：{error}")))?;
        let config = source.parse_with_remote(&content, self.section.format.to_file_format())?;
        config.validate()?;
        Ok(config)
    }
}

/// 配置热更新器（轮询 Nacos）；**drop 即停止**。
pub struct NacosWatcher {
    task: tokio::task::JoinHandle<()>,
    client: NacosClient,
    handle: ConfigHandle,
    /// 本地来源：与启动时用的**同一个** `ConfigSource`，保证重放一致。
    source: ConfigSource,
}

impl NacosWatcher {
    /// 按 `[nacos]` 配置启动轮询。
    ///
    /// `source` 是启动时用的那套本地来源：远端内容叠加在它之上，因此热更新与
    /// 启动看到的是同一份本地基线。
    ///
    /// # Errors
    /// 未启用 `[nacos].enabled` 或客户端创建失败时返回错误。
    pub async fn start(handle: ConfigHandle, source: ConfigSource) -> AppResult<Self> {
        let section = handle.load().nacos.clone();
        if !section.enabled {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                "[nacos].enabled = false，无需启动配置中心监听",
            ));
        }

        let client = NacosClient::new(&section)?;
        let interval = section.poll_interval;
        let poller = client.clone();
        let watched = handle.clone();
        let reload_source = source.clone();

        let task = tokio::spawn(async move {
            // 不预先拉一次做基线：若内容在 start 与首次轮询之间变化，
            // 预拉会把「新内容」当成基线而永远不触发重载。
            // 首次轮询总会 load 一次（内容与基线相同则后续跳过）。
            let mut last: Option<String> = None;
            tracing::info!(
                server = %poller.section.server_addr,
                data_id = %poller.section.data_id,
                ?interval,
                "Nacos 配置监听已启动"
            );

            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // 首个 tick 立即返回，跳过（刚拉过）

            loop {
                ticker.tick().await;
                match poller.fetch().await {
                    Ok(content) => {
                        if last.as_deref() == Some(content.as_str()) {
                            continue;
                        }
                        match poller.load_config(&reload_source).await {
                            Ok(reloaded) => {
                                // 安全边界只可收紧：放宽会被拒绝，此时保留旧配置
                                if let Err(reason) = watched.store(reloaded) {
                                    tracing::warn!(
                                        %reason,
                                        "Nacos 配置热更新被拒绝（会放宽安全边界），保留旧配置"
                                    );
                                    last = Some(content);
                                    continue;
                                }
                                last = Some(content);
                                tracing::info!("Nacos 配置已热更新（仅对新请求生效）");
                            }
                            Err(error) => {
                                // fail-safe：坏配置保留旧值
                                tracing::warn!(%error, "Nacos 配置解析/校验失败，保留旧配置");
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Nacos 轮询失败，下个周期重试");
                    }
                }
            }
        });

        Ok(Self {
            task,
            client,
            handle,
            source,
        })
    }

    /// 立即拉取一次（手动触发 / 测试用）。
    ///
    /// # Errors
    /// 拉取或校验失败时返回错误，**旧配置保持不变**。
    pub async fn reload_now(&self) -> AppResult<()> {
        let reloaded = self
            .client
            .load_config(&self.source)
            .await
            .map_err(|error| {
                // 上游错误文本含内网地址 / 端口 / 请求路径，只进日志
                tracing::error!(%error, "Nacos 配置重载失败");
                AppError::biz(
                    ErrorCode::UPSTREAM_ERROR,
                    ErrorCode::UPSTREAM_ERROR.default_message(),
                )
            })?;
        self.handle
            .store(reloaded)
            .map_err(|reason| AppError::biz(ErrorCode::UPSTREAM_ERROR, reason))
    }

    /// 当前配置快照。
    #[must_use]
    pub fn current(&self) -> std::sync::Arc<AppConfig> {
        self.handle.load()
    }
}

impl Drop for NacosWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for NacosWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NacosWatcher")
            .field("finished", &self.task.is_finished())
            .finish_non_exhaustive()
    }
}

fn net_err(error: reqwest::Error) -> AppError {
    // `Display` 会带上完整 URL，而 `accessToken` 是 query 参数 → 先去掉 URL 再进日志
    let error = error.without_url();
    tracing::warn!(%error, "Nacos HTTP 请求失败");
    AppError::biz(ErrorCode::UPSTREAM_ERROR, "配置中心不可用")
}

/// 远端配置内容的大小上限（1 MiB）。
///
/// 配置中心异常或被攻破时可能返回超大响应，无上限会把进程内存打满。
const MAX_REMOTE_CONFIG_BYTES: usize = 1024 * 1024;

/// 带上限读取响应体。
async fn read_limited(mut response: reqwest::Response, limit: usize) -> AppResult<String> {
    if let Some(len) = response.content_length()
        && len > limit as u64
    {
        return Err(AppError::biz(
            ErrorCode::UPSTREAM_ERROR,
            format!("Nacos 配置过大：Content-Length {len} > {limit}"),
        ));
    }

    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(net_err)? {
        if buf.len() + chunk.len() > limit {
            return Err(AppError::biz(
                ErrorCode::UPSTREAM_ERROR,
                format!("Nacos 配置超过 {limit} 字节上限"),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf)
        .map_err(|_| AppError::biz(ErrorCode::UPSTREAM_ERROR, "Nacos 配置不是合法 UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_trim_trailing_slash_from_base() {
        let section = NacosSection {
            server_addr: "http://127.0.0.1:8848/".to_string(),
            ..Default::default()
        };
        let client = NacosClient::new(&section).expect("client");
        assert_eq!(client.base(), "http://127.0.0.1:8848");
    }

    #[test]
    fn should_build_query_with_data_id_and_group() {
        let section = NacosSection {
            data_id: "aurora.toml".to_string(),
            group: "MY_GROUP".to_string(),
            ..Default::default()
        };
        let query = section.config_query();
        assert!(
            query
                .iter()
                .any(|(k, v)| *k == "dataId" && v == "aurora.toml")
        );
        assert!(query.iter().any(|(k, v)| *k == "group" && v == "MY_GROUP"));
    }
}
