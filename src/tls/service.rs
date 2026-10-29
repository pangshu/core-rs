//! TLS 服务：证书仓库、SNI 解析、到期告警与 OCSP 装订的装配。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::config::TlsSection;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::tls::cert;
#[cfg(feature = "ocsp-stapling")]
use crate::tls::ocsp;
use crate::tls::reload;
use crate::tls::store::{CertStore, LoadOptions, wildcard_match};

/// 证书仓库的共享句柄：热更新时整体替换内层 `Arc<CertStore>`。
pub(crate) type StoreHandle = Arc<RwLock<Arc<CertStore>>>;

/// 到期检查周期。
const EXPIRY_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// TLS 支持的 ALPN 协议（不设的话 HTTPS 永远协商不到 h2）。
const ALPN_PROTOCOLS: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// 证书整体替换的句柄。
///
/// **替换证书与丢弃 OCSP 装订是同一件事**：装订响应是签给旧证书的，只换 store
/// 不换缓存，`SniResolver` 会继续返回缓存里那份带旧证书链的 `CertifiedKey`
/// （见 `docs/16` §10）。因此所有重载路径都必须走 [`apply`](Self::apply)。
#[derive(Clone)]
pub(crate) struct CertSwap {
    store: StoreHandle,
    dir: PathBuf,
    opts: LoadOptions,
    #[cfg(feature = "ocsp-stapling")]
    ocsp: Option<Arc<ocsp::OcspManager>>,
}

impl CertSwap {
    /// 重新从目录加载（阻塞 IO，热更新任务里请用 `spawn_blocking` 包一层）。
    pub(crate) fn load(&self) -> AppResult<CertStore> {
        CertStore::from_dir_with(&self.dir, self.opts)
    }

    /// 原子替换证书仓库，并丢弃与之绑定的 OCSP 缓存；返回新仓库里的证书数量。
    pub(crate) fn apply(&self, reloaded: CertStore) -> usize {
        let count = reloaded.entries().len();
        match self.store.write() {
            Ok(mut guard) => *guard = Arc::new(reloaded),
            Err(poisoned) => *poisoned.into_inner() = Arc::new(reloaded),
        }
        #[cfg(feature = "ocsp-stapling")]
        if let Some(manager) = &self.ocsp {
            manager.invalidate();
        }
        count
    }

    /// 当前仓库快照。
    pub(crate) fn snapshot(&self) -> Arc<CertStore> {
        match self.store.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// TLS 服务（`serve_tls` 的输入）。
///
/// `Clone` 廉价：内部共享同一份证书仓库。
#[derive(Clone)]
pub struct TlsService {
    swap: CertSwap,
    warn_days: u32,
    /// SNI 白名单（`[tls].domains`，已转小写）；为空表示不限制。
    domains: Vec<String>,
    handshake_timeout: Duration,
}

impl TlsService {
    /// 按 `[tls]` 装配：加载证书 → 到期检查 → 启动热更新监听。
    ///
    /// # Errors
    /// `enabled = false`、证书目录缺失、证书解析/配对失败、或配置校验不通过时返回错误。
    pub fn from_config(cfg: &TlsSection) -> AppResult<Self> {
        if !cfg.enabled {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                "[tls].enabled = false，无法启动 HTTPS",
            ));
        }

        let dir = PathBuf::from(&cfg.cert_dir);
        let opts = LoadOptions {
            strict_key_perms: cfg.strict_key_perms,
        };
        // 先加载：目录缺失/证书坏了要报 `50005`（与历史行为一致），
        // 再跑配置校验把「参数类」错误归到 `10001`。
        let store = CertStore::from_dir_with(&dir, opts)?;
        cfg.validate()
            .map_err(|message| AppError::biz(ErrorCode::INVALID_PARAM, message))?;

        let domains: Vec<String> = cfg
            .domains
            .iter()
            .map(|domain| domain.to_ascii_lowercase())
            .collect();
        check_domain_coverage(&dir, &domains, &store);

        #[cfg_attr(not(feature = "ocsp-stapling"), allow(unused_mut))]
        let mut service = Self {
            swap: CertSwap {
                store: Arc::new(RwLock::new(Arc::new(store))),
                dir,
                opts,
                #[cfg(feature = "ocsp-stapling")]
                ocsp: None,
            },
            warn_days: cfg.expiry_warn_days,
            domains,
            handshake_timeout: cfg.handshake_timeout,
        };

        // OCSP 装订：构造管理器并启动后台刷新（首个 tick 立即执行）
        #[cfg(feature = "ocsp-stapling")]
        if cfg.ocsp.enabled {
            let manager = Arc::new(ocsp::OcspManager::new(&cfg.ocsp)?);
            manager.spawn(Arc::clone(&service.swap.store));
            service.swap.ocsp = Some(manager);
            tracing::info!(
                refresh = ?cfg.ocsp.refresh_interval,
                "OCSP 装订已启用（首轮拉取进行中）"
            );
        }

        service.log_expiring();
        if cfg.reload {
            reload::spawn(service.swap.clone());
        }
        service.spawn_expiry_monitor();
        Ok(service)
    }

    /// 已装订的证书数量（`0` 表示未启用 OCSP 或尚未拉到响应）。
    #[cfg(feature = "ocsp-stapling")]
    #[must_use]
    pub fn ocsp_stapled_count(&self) -> usize {
        self.swap
            .ocsp
            .as_ref()
            .map_or(0, |manager| manager.stapled_count())
    }

    /// OCSP 拉取的失败记录（域名 → 原因；成功后自动清除）。
    #[cfg(feature = "ocsp-stapling")]
    #[must_use]
    pub fn ocsp_last_errors(&self) -> Vec<(String, String)> {
        self.swap
            .ocsp
            .as_ref()
            .map(|manager| manager.last_errors())
            .unwrap_or_default()
    }

    /// 当前证书仓库快照。
    #[must_use]
    pub fn store(&self) -> Arc<CertStore> {
        self.swap.snapshot()
    }

    /// TLS 握手超时（`[tls].handshake_timeout`）。
    #[must_use]
    pub fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }

    /// 立即重载证书（热更新任务也走同一条路径）。
    ///
    /// # Errors
    /// 新证书解析失败时返回错误，**旧证书保持不变**。
    pub fn reload(&self) -> AppResult<()> {
        let reloaded = self.swap.load()?;
        let count = self.swap.apply(reloaded);
        tracing::info!(
            certs = count,
            "证书已重载（OCSP 装订缓存已丢弃，仅对新连接生效）"
        );
        Ok(())
    }

    /// 构造 rustls 配置（带 SNI 解析器）。
    #[must_use]
    pub fn server_config(&self) -> Arc<ServerConfig> {
        let resolver = Arc::new(SniResolver {
            store: Arc::clone(&self.swap.store),
            domains: self.domains.clone(),
            #[cfg(feature = "ocsp-stapling")]
            ocsp: self.swap.ocsp.clone(),
        });
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        // `axum-server` 的 `RustlsConfig::from_config` 不会替我们设 ALPN，
        // 不设就只能跑 HTTP/1.1（`[server]` 的 HTTP/2 配置在 TLS 入口静默失效）。
        config.alpn_protocols = ALPN_PROTOCOLS
            .iter()
            .map(|protocol| protocol.to_vec())
            .collect();
        Arc::new(config)
    }

    /// 记录即将到期的证书（启动期与每 6 小时各一次）。
    pub fn log_expiring(&self) {
        let now = cert::now_unix();
        for entry in self.store().entries() {
            let Some(not_after) = entry.not_after else {
                continue;
            };
            let days = cert::days_until(not_after, now);
            if cert::should_warn(Some(not_after), self.warn_days, now) {
                tracing::warn!(
                    domain = %if entry.pattern.is_empty() { "<default>" } else { &entry.pattern },
                    days_left = days,
                    "证书即将到期，请及时续期（见 docs/16 §7）"
                );
            } else {
                tracing::debug!(
                    domain = %if entry.pattern.is_empty() { "<default>" } else { &entry.pattern },
                    days_left = days,
                    "证书有效期正常"
                );
            }
        }
    }

    fn spawn_expiry_monitor(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(EXPIRY_CHECK_INTERVAL);
            ticker.tick().await; // 首次立即返回，跳过（启动时已检查过）
            loop {
                ticker.tick().await;
                service.log_expiring();
            }
        });
    }
}

impl std::fmt::Debug for TlsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsService")
            .field("dir", &self.swap.dir)
            .field("warn_days", &self.warn_days)
            .field("domains", &self.domains)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("certs", &self.store().entries().len())
            .field("ocsp_stapled", &{
                #[cfg(feature = "ocsp-stapling")]
                {
                    self.swap
                        .ocsp
                        .as_ref()
                        .map_or(0, |manager| manager.stapled_count())
                }
                #[cfg(not(feature = "ocsp-stapling"))]
                {
                    0
                }
            })
            .finish()
    }
}

/// 启动期交叉校验 `[tls].domains` 与证书目录，把「配了但永远不生效」提前暴露。
fn check_domain_coverage(dir: &Path, domains: &[String], store: &CertStore) {
    if domains.is_empty() {
        tracing::warn!(
            dir = %dir.display(),
            "[tls].domains 为空：任意 SNI 都会拿到默认证书（可被域前置借用），\
             建议显式列出对外域名"
        );
        return;
    }

    for domain in domains {
        let covered = store
            .entries()
            .iter()
            .any(|entry| entry.pattern == *domain || wildcard_match(&entry.pattern, domain));
        if !covered {
            tracing::warn!(
                domain = %domain,
                "该域名在证书目录里没有对应证书：命中时只能回退默认证书（名字可能不匹配）"
            );
        }
    }

    for entry in store.entries() {
        if entry.pattern.is_empty() {
            continue;
        }
        if !domains.iter().any(|domain| domain == &entry.pattern) {
            tracing::warn!(
                pattern = %entry.pattern,
                "[tls].domains 未包含该证书的 SNI 键：这张证书不会被下发"
            );
        }
    }
}

/// SNI 解析器：每次握手读一次当前证书仓库。
#[derive(Debug)]
struct SniResolver {
    store: StoreHandle,
    /// 非空时是**白名单**：未命中直接拒绝握手（`None`）。
    domains: Vec<String>,
    #[cfg(feature = "ocsp-stapling")]
    ocsp: Option<Arc<ocsp::OcspManager>>,
}

impl SniResolver {
    /// SNI 是否被 `[tls].domains` 允许（列表为空 = 不限制）。
    fn allows(&self, server_name: &str) -> bool {
        if self.domains.is_empty() {
            return true;
        }
        let name = server_name.to_ascii_lowercase();
        self.domains
            .iter()
            .any(|pattern| pattern == &name || wildcard_match(pattern, &name))
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let store = match self.store.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        };

        let server_name = client_hello.server_name();
        if let Some(name) = server_name
            && !self.allows(name)
        {
            // 不做「未授权 SNI 也发默认证书」：那等于允许任意域名借用本站证书
            tracing::debug!(sni = %name, "[tls].domains 未包含该 SNI，拒绝握手");
            return None;
        }

        let entry = store.resolve_entry(server_name)?;

        // 命中装订缓存就返回带 `ocsp` 的证书；未命中回退到未装订版本
        #[cfg(feature = "ocsp-stapling")]
        if let Some(manager) = &self.ocsp
            && let Some(key) = manager.resolve(entry)
        {
            return Some(key);
        }

        Some(Arc::clone(&entry.key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_refuse_when_disabled() {
        let error = TlsService::from_config(&TlsSection::default()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
    }

    #[test]
    fn should_fail_with_missing_cert_dir() {
        let cfg = TlsSection {
            enabled: true,
            cert_dir: "no/such/certs".to_string(),
            ..Default::default()
        };
        let error = TlsService::from_config(&cfg).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn should_allow_any_sni_without_domains() {
        let resolver = SniResolver {
            store: Arc::new(RwLock::new(Arc::new(CertStore::default()))),
            domains: Vec::new(),
            #[cfg(feature = "ocsp-stapling")]
            ocsp: None,
        };
        assert!(resolver.allows("anything.evil.com"));
    }

    #[test]
    fn should_enforce_domains_as_allowlist() {
        let resolver = SniResolver {
            store: Arc::new(RwLock::new(Arc::new(CertStore::default()))),
            domains: vec!["api.example.com".to_string(), "*.example.org".to_string()],
            #[cfg(feature = "ocsp-stapling")]
            ocsp: None,
        };
        assert!(resolver.allows("api.example.com"));
        assert!(resolver.allows("API.EXAMPLE.COM"), "大小写不敏感");
        assert!(resolver.allows("www.example.org"), "一级通配");
        assert!(!resolver.allows("a.b.example.org"), "通配符不跨级");
        assert!(!resolver.allows("anything.evil.com"));
    }
}
