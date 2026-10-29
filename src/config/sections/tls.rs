//! `[tls]` 配置段（feature `tls`）。
//!
//! 字段与默认值见 `docs/08` §3 `[tls]`，证书布局与运维流程见 `docs/16`。

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::config::sections::de::de_duration;

/// 服务端 TLS 配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct TlsSection {
    /// 是否启用 HTTPS。
    pub enabled: bool,
    /// 证书目录（布局见 `docs/16` §2）。
    pub cert_dir: String,
    /// **SNI 白名单**：非空时只服务这些域名（支持 `*.example.com`，只匹配一级子域），
    /// 未命中的 SNI 直接拒绝握手；为空表示不限制（任意 SNI 回退默认证书）。
    pub domains: Vec<String>,
    /// 证书文件变更时是否热更新。
    pub reload: bool,
    /// 到期告警阈值（天）。
    pub expiry_warn_days: u32,
    /// TLS 握手超时：防止慢速 ClientHello 长期占用 fd / 内存。
    #[serde(deserialize_with = "de_duration")]
    pub handshake_timeout: Duration,
    /// 私钥文件权限过宽（同组 / 其他用户可读）时是否拒绝加载（仅 Unix）。
    pub strict_key_perms: bool,
    /// OCSP Stapling（feature `ocsp-stapling`）。
    pub ocsp: OcspSection,
}

impl Default for TlsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_dir: "certs".to_string(),
            domains: Vec::new(),
            reload: true,
            expiry_warn_days: 30,
            handshake_timeout: Duration::from_secs(10),
            strict_key_perms: true,
            ocsp: OcspSection::default(),
        }
    }
}

/// OCSP Stapling 配置（`[tls.ocsp]`，feature `ocsp-stapling`）。
///
/// 默认**关闭**：OCSP 会让服务端额外依赖 CA 的 responder（可用性、隐私与延迟都有代价），
/// 决策依据见 `docs/16` §10。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct OcspSection {
    /// 是否启用（需 feature `ocsp-stapling`）。
    pub enabled: bool,
    /// 覆盖 responder 地址；留空则用证书 AIA 扩展里的 `id-ad-ocsp`。
    pub responder: String,
    /// 单次请求超时。
    #[serde(deserialize_with = "de_duration")]
    pub timeout: Duration,
    /// 后台刷新周期（每轮检查哪些证书需要重新拉取）。
    #[serde(deserialize_with = "de_duration")]
    pub refresh_interval: Duration,
    /// 提前刷新窗口：响应剩余有效期低于该值就重新拉取。
    #[serde(deserialize_with = "de_duration")]
    pub refresh_ahead: Duration,
    /// 允许的时钟偏移（`thisUpdate` 在未来、`nextUpdate` 刚过期时的容忍度）。
    #[serde(deserialize_with = "de_duration")]
    pub max_skew: Duration,
    /// 是否允许明文 `http://` 的 responder。
    ///
    /// 明文 responder 只能保证响应**完整性**（签名），保证不了**新鲜度**：
    /// 链路上的中间人可以丢弃请求（软失败）或重放仍在有效期内的旧响应，
    /// 从而压制吊销状态。显式配置 `http://` 必须打开这个开关。
    pub allow_insecure: bool,
    /// 单次响应体上限（字节）：防止被控 responder 用超大响应打爆内存。
    pub max_response_bytes: usize,
}

impl Default for OcspSection {
    fn default() -> Self {
        Self {
            enabled: false,
            responder: String::new(),
            timeout: Duration::from_secs(5),
            refresh_interval: Duration::from_secs(15 * 60),
            refresh_ahead: Duration::from_secs(60 * 60),
            max_skew: Duration::from_secs(5 * 60),
            allow_insecure: false,
            max_response_bytes: 64 * 1024,
        }
    }
}

impl OcspSection {
    /// 启动期校验（fail-fast）。
    ///
    /// # Errors
    /// 启用但未编译 `ocsp-stapling`、刷新周期过短、或超时为 0 时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !cfg!(feature = "ocsp-stapling") {
            return Err("[tls].ocsp.enabled = true 需要启用 `ocsp-stapling` feature".to_string());
        }
        if self.timeout.is_zero() {
            return Err("[tls].ocsp.timeout 必须大于 0".to_string());
        }
        if self.refresh_interval < MIN_REFRESH_INTERVAL {
            return Err(format!(
                "[tls].ocsp.refresh_interval 不能小于 {MIN_REFRESH_INTERVAL:?}\
                 （否则会把 CA 的 responder 打爆）"
            ));
        }
        if self.refresh_ahead >= self.refresh_interval * 24 {
            return Err(
                "[tls].ocsp.refresh_ahead 过大（应小于 refresh_interval 的 24 倍）".to_string(),
            );
        }
        if !self.responder.trim().is_empty() {
            let responder = self.responder.trim();
            if !responder.starts_with("http://") && !responder.starts_with("https://") {
                return Err(format!(
                    "[tls].ocsp.responder = \"{responder}\" 必须以 http:// 或 https:// 开头"
                ));
            }
            if responder.starts_with("http://") && !self.allow_insecure {
                return Err(format!(
                    "[tls].ocsp.responder = \"{responder}\" 是明文 http://：\
                     中间人可丢弃请求或重放旧响应来压制吊销状态。\
                     确实要在可信网络内走明文，请显式设置 [tls.ocsp].allow_insecure = true"
                ));
            }
        }
        if !(MIN_RESPONSE_BYTES..=MAX_RESPONSE_BYTES).contains(&self.max_response_bytes) {
            return Err(format!(
                "[tls.ocsp].max_response_bytes = {} 超出范围（{MIN_RESPONSE_BYTES}..={MAX_RESPONSE_BYTES} 字节）",
                self.max_response_bytes
            ));
        }
        Ok(())
    }
}

/// 刷新周期下限。
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// 响应体上限下界（正常 OCSP 响应 < 8 KiB）。
pub const MIN_RESPONSE_BYTES: usize = 1024;
/// 响应体上限上界。
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// 握手超时下界。
pub const MIN_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
/// 握手超时上界（再长就等于没有超时）。
pub const MAX_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

impl TlsSection {
    /// 启动期校验（fail-fast）。
    ///
    /// # Errors
    /// 启用但证书目录不存在、域名为空或格式非法时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.cert_dir.trim().is_empty() {
            return Err("[tls].cert_dir 不能为空".to_string());
        }
        if !Path::new(&self.cert_dir).is_dir() {
            return Err(format!(
                "[tls].cert_dir = \"{}\" 不是目录（见 docs/16 §2 的证书布局）",
                self.cert_dir
            ));
        }
        if self.expiry_warn_days == 0 || self.expiry_warn_days > 365 {
            return Err(
                "[tls].expiry_warn_days 必须在 1..=365 之间（0 等于关掉到期告警）".to_string(),
            );
        }
        if !(MIN_HANDSHAKE_TIMEOUT..=MAX_HANDSHAKE_TIMEOUT).contains(&self.handshake_timeout) {
            return Err(format!(
                "[tls].handshake_timeout = {:?} 超出范围（{MIN_HANDSHAKE_TIMEOUT:?}..={MAX_HANDSHAKE_TIMEOUT:?}）",
                self.handshake_timeout
            ));
        }
        for domain in &self.domains {
            validate_domain(domain)?;
        }
        self.ocsp.validate()?;
        Ok(())
    }
}

/// 校验域名模式：`example.com` / `api.example.com` / `*.example.com` / `localhost`。
fn validate_domain(domain: &str) -> Result<(), String> {
    let host = domain.strip_prefix("*.").unwrap_or(domain);
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return Err(format!("[tls].domains 中的 \"{domain}\" 不是合法域名"));
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
    {
        return Err(format!(
            "[tls].domains 中的 \"{domain}\" 含非法字符（只允许字母数字、`-`、`.`）"
        ));
    }
    if host.split('.').any(str::is_empty) {
        return Err(format!(
            "[tls].domains 中的 \"{domain}\" 含空标签（如 `example..com`）"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_disabled() {
        let s = TlsSection::default();
        assert!(!s.enabled);
        assert_eq!(s.cert_dir, "certs");
        assert!(s.reload);
        assert_eq!(s.expiry_warn_days, 30);
    }

    #[test]
    fn should_accept_wildcard_domain() {
        assert!(validate_domain("*.example.com").is_ok());
        assert!(validate_domain("api.example.com").is_ok());
        assert!(validate_domain("localhost").is_ok());
    }

    #[test]
    fn should_reject_unknown_keys() {
        // 典型误配：`enable` 少个 d。以前会被静默忽略——TLS 根本没开，日志里
        // 也没有任何提示；现在必须在加载期报错。
        let parsed: Result<TlsSection, _> = serde_json::from_str(r#"{ "enable": true }"#);
        assert!(parsed.is_err(), "未知键必须报错：{parsed:?}");

        let parsed: Result<OcspSection, _> = serde_json::from_str(r#"{ "enabledd": true }"#);
        assert!(parsed.is_err(), "未知键必须报错");
    }

    #[test]
    fn should_reject_bad_domain() {
        assert!(validate_domain("").is_err());
        assert!(validate_domain("*.example..com").is_err());
        assert!(validate_domain("bad domain").is_err());
    }

    #[test]
    fn should_fail_when_enabled_without_cert_dir() {
        let s = TlsSection {
            enabled: true,
            cert_dir: "no/such/dir".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_default_ocsp_off() {
        let ocsp = OcspSection::default();
        assert!(!ocsp.enabled);
        assert!(ocsp.responder.is_empty());
        assert_eq!(ocsp.timeout, Duration::from_secs(5));
        assert_eq!(ocsp.refresh_interval, Duration::from_secs(900));
    }

    #[test]
    fn should_skip_ocsp_validation_when_disabled() {
        // 未启用时即使 feature 没开也不应报错
        assert!(OcspSection::default().validate().is_ok());
    }

    #[test]
    fn should_reject_zero_ocsp_timeout() {
        let s = OcspSection {
            enabled: true,
            timeout: Duration::ZERO,
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_too_short_ocsp_refresh() {
        let s = OcspSection {
            enabled: true,
            refresh_interval: Duration::from_secs(5),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_ocsp_responder_without_scheme() {
        let s = OcspSection {
            enabled: true,
            responder: "ocsp.example.com".to_string(),
            ..Default::default()
        };
        assert!(s.validate().is_err());

        let ok = OcspSection {
            enabled: true,
            responder: "http://ocsp.example.com".to_string(),
            allow_insecure: true,
            ..Default::default()
        };
        // feature 未开时仍会因 feature 报错，开了则应通过
        if cfg!(feature = "ocsp-stapling") {
            assert!(ok.validate().is_ok(), "{:?}", ok.validate());
        }
    }

    #[test]
    fn should_require_explicit_opt_in_for_plaintext_responder() {
        // 明文 responder 挡不住「丢弃请求 / 重放旧响应」，必须显式打开
        let s = OcspSection {
            enabled: true,
            responder: "http://ocsp.example.com".to_string(),
            ..Default::default()
        };
        // 未编译 `ocsp-stapling` 时会先报 feature 缺失
        if cfg!(feature = "ocsp-stapling") {
            let error = s.validate().unwrap_err();
            assert!(error.contains("allow_insecure"), "实际：{error}");

            let https = OcspSection {
                enabled: true,
                responder: "https://ocsp.example.com".to_string(),
                ..Default::default()
            };
            assert!(https.validate().is_ok(), "{:?}", https.validate());
        }
    }

    #[test]
    fn should_reject_out_of_range_response_limit() {
        if !cfg!(feature = "ocsp-stapling") {
            return; // 未编译 OCSP 时校验会先报 feature 缺失
        }
        let too_small = OcspSection {
            enabled: true,
            max_response_bytes: 16,
            ..Default::default()
        };
        assert!(too_small.validate().is_err());
        let too_large = OcspSection {
            enabled: true,
            max_response_bytes: 8 * 1024 * 1024,
            ..Default::default()
        };
        assert!(too_large.validate().is_err());
    }

    #[test]
    fn should_reject_zero_expiry_warning_and_bad_handshake_timeout() {
        let dir = std::env::temp_dir();
        let base = TlsSection {
            enabled: true,
            cert_dir: dir.to_string_lossy().into_owned(),
            ..Default::default()
        };

        let no_warning = TlsSection {
            expiry_warn_days: 0,
            ..base.clone()
        };
        assert!(no_warning.validate().is_err(), "0 等于关掉到期告警");

        let slow = TlsSection {
            handshake_timeout: Duration::ZERO,
            ..base.clone()
        };
        assert!(slow.validate().is_err());

        let too_slow = TlsSection {
            handshake_timeout: Duration::from_secs(600),
            ..base
        };
        assert!(too_slow.validate().is_err(), "超时太长等于没有超时");
    }

    #[test]
    fn should_require_ocsp_feature_when_enabled() {
        let s = OcspSection {
            enabled: true,
            ..Default::default()
        };
        if cfg!(feature = "ocsp-stapling") {
            assert!(s.validate().is_ok());
        } else {
            let error = s.validate().unwrap_err();
            assert!(error.contains("ocsp-stapling"), "实际：{error}");
        }
    }
}
