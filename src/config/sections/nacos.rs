//! `[nacos]` 配置段（feature `nacos`）。
//!
//! 配置中心（Nacos）的取值约定：远端内容按 `format` 解析后，**覆盖本地文件、
//! 被环境变量覆盖**（优先级见 `docs/08` §2）。

use std::time::Duration;

use config::FileFormat;
use serde::Deserialize;

use crate::config::sections::de::de_duration;

/// 轮询间隔下限（避免把 Nacos 打爆）。
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Nacos 配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct NacosSection {
    /// 是否启用配置中心。
    pub enabled: bool,
    /// 服务地址（如 `http://127.0.0.1:8848`）。
    pub server_addr: String,
    /// 配置 Data ID（如 `aurora.toml`）。
    pub data_id: String,
    /// 配置分组。
    pub group: String,
    /// 命名空间 id（`public` 留空）。
    pub namespace: String,
    /// 用户名（开启鉴权时必填）。
    pub username: String,
    /// 密码。
    pub password: String,
    /// 远端内容格式。
    pub format: NacosFormat,
    /// 单次请求超时。
    #[serde(deserialize_with = "de_duration")]
    pub timeout: Duration,
    /// 轮询间隔（热更新）。
    #[serde(deserialize_with = "de_duration")]
    pub poll_interval: Duration,
}

impl Default for NacosSection {
    fn default() -> Self {
        Self {
            enabled: false,
            server_addr: "http://127.0.0.1:8848".to_string(),
            data_id: "aurora.toml".to_string(),
            group: "DEFAULT_GROUP".to_string(),
            namespace: String::new(),
            username: String::new(),
            password: String::new(),
            format: NacosFormat::Toml,
            timeout: Duration::from_secs(3),
            poll_interval: Duration::from_secs(30),
        }
    }
}

impl NacosSection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// 启用但地址 / Data ID / 分组非法，或轮询间隔为 0 时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// 远端配置是**信任输入**（会覆盖本地配置），因此对传输通道有要求：
    /// - 配了 `username` 时**必须** `https://`（否则凭据明文上网）；
    /// - 非本机地址用明文 `http://`：生产拒绝启动，其它环境告警。
    ///
    /// # Errors
    /// 地址非法、明文传输不满足要求、Data ID / 分组为空、轮询间隔过短时返回错误。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let plaintext = self.server_addr.starts_with("http://");
        if !plaintext && !self.server_addr.starts_with("https://") {
            return Err(format!(
                "[nacos].server_addr = \"{}\" 必须以 http:// 或 https:// 开头",
                self.server_addr
            ));
        }
        let host = self.host()?;
        let loopback = is_loopback_host(&host);

        if plaintext && !self.username.trim().is_empty() {
            return Err(format!(
                "[nacos].server_addr = \"{}\" 使用明文 http://，但已配置 username：\
                 凭据会以明文发送，请改用 https://",
                self.server_addr
            ));
        }
        if plaintext && !loopback {
            if production {
                return Err(format!(
                    "[nacos].server_addr = \"{}\" 在生产环境必须是 https://：\
                     明文通道下配置内容可被中间人篡改（含 [tls] / [auth] 等段）",
                    self.server_addr
                ));
            }
            tracing::warn!(
                addr = %self.server_addr,
                "[nacos] 使用明文 http:// 访问非本机 Nacos：配置内容可被中间人篡改"
            );
        }
        if self.data_id.trim().is_empty() {
            return Err("[nacos].data_id 不能为空".to_string());
        }
        if self.group.trim().is_empty() {
            return Err("[nacos].group 不能为空".to_string());
        }
        if self.poll_interval < MIN_POLL_INTERVAL {
            return Err(format!(
                "[nacos].poll_interval 不能小于 {MIN_POLL_INTERVAL:?}"
            ));
        }
        Ok(())
    }

    /// 解析 `server_addr` 的 host（用于判断是否本机）。
    fn host(&self) -> Result<String, String> {
        reqwest::Url::parse(&self.server_addr)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .ok_or_else(|| {
                format!(
                    "[nacos].server_addr = \"{}\" 不是合法 URL",
                    self.server_addr
                )
            })
    }

    /// 拼 `GET /nacos/v1/cs/configs` 的查询串（不含 accessToken）。
    #[must_use]
    pub fn config_query(&self) -> Vec<(&'static str, String)> {
        let mut query = vec![
            ("dataId", self.data_id.clone()),
            ("group", self.group.clone()),
        ];
        if !self.namespace.trim().is_empty() {
            query.push(("tenant", self.namespace.clone()));
        }
        query
    }
}

/// 远端配置内容格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum NacosFormat {
    /// TOML（默认）。
    #[default]
    Toml,
    /// YAML。
    Yaml,
    /// JSON。
    Json,
}

impl NacosFormat {
    /// 转为 `config` crate 的文件格式。
    #[must_use]
    pub fn to_file_format(self) -> FileFormat {
        match self {
            Self::Toml => FileFormat::Toml,
            Self::Yaml => FileFormat::Yaml,
            Self::Json => FileFormat::Json,
        }
    }
}

/// 是否本机地址（`localhost` / `127.0.0.0/8` / `::1`）。
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_disabled() {
        let s = NacosSection::default();
        assert!(!s.enabled);
        assert_eq!(s.group, "DEFAULT_GROUP");
        assert_eq!(s.format, NacosFormat::Toml);
        assert_eq!(s.poll_interval, Duration::from_secs(30));
    }

    #[test]
    fn should_reject_bad_server_addr() {
        let s = NacosSection {
            enabled: true,
            server_addr: "127.0.0.1:8848".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_empty_data_id() {
        let s = NacosSection {
            enabled: true,
            data_id: String::new(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_too_short_poll_interval() {
        let s = NacosSection {
            enabled: true,
            poll_interval: Duration::from_millis(100),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_include_tenant_only_when_namespace_set() {
        let s = NacosSection::default();
        assert!(!s.config_query().iter().any(|(k, _)| *k == "tenant"));

        let s = NacosSection {
            namespace: "ns-1".to_string(),
            ..Default::default()
        };
        assert!(
            s.config_query()
                .iter()
                .any(|(k, v)| *k == "tenant" && v == "ns-1")
        );
    }

    fn enabled_at(addr: &str) -> NacosSection {
        NacosSection {
            enabled: true,
            server_addr: addr.to_string(),
            ..NacosSection::default()
        }
    }

    #[test]
    fn should_allow_plaintext_only_for_loopback() {
        // 本机：任何环境都允许明文
        assert!(
            enabled_at("http://127.0.0.1:8848")
                .validate_for_env(true)
                .is_ok()
        );
        assert!(
            enabled_at("http://localhost:8848")
                .validate_for_env(true)
                .is_ok()
        );
        assert!(
            enabled_at("http://[::1]:8848")
                .validate_for_env(true)
                .is_ok()
        );
    }

    #[test]
    fn should_reject_plaintext_remote_nacos_in_production() {
        let s = enabled_at("http://nacos.internal:8848");
        // 非生产只告警
        assert!(s.validate_for_env(false).is_ok());
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("https://"), "实际：{error}");

        assert!(
            enabled_at("https://nacos.internal:8848")
                .validate_for_env(true)
                .is_ok()
        );
    }

    #[test]
    fn should_require_https_when_credentials_are_configured() {
        let mut s = enabled_at("http://nacos.internal:8848");
        s.username = "nacos".to_string();
        // 即使非生产也不允许明文传凭据
        let error = s.validate_for_env(false).unwrap_err();
        assert!(error.contains("username"), "实际：{error}");
    }
}
