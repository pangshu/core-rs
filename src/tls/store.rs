//! 证书仓库与 SNI 选择。
//!
//! 目录布局（`docs/16` §2）：
//!
//! ```text
//! certs/
//! ├── fullchain.pem          # 默认证书链（未匹配 SNI 时回退）
//! ├── privkey.pem
//! └── domains/
//!     ├── api.example.com.pem
//!     ├── api.example.com.key
//!     ├── *.example.com.pem   # 通配符（只匹配一级子域）
//!     └── *.example.com.key
//! ```
//!
//! 证书**由业务提供**（框架不签发）；`CertProvider` 预留给 DB / KMS 等来源。
//!
//! 加载期做四道自检（任一不过就**拒绝加载**，避免「看起来起来了、握手全失败」）：
//! 1. 私钥与证书必须配对（`CertifiedKey::from_der` 的 `keys_match`）；
//! 2. 链首必须是叶子证书（`basicConstraints.cA = false`）；
//! 3. `domains/*.pem` 的文件名（SNI 键）必须被证书 SAN 覆盖（不覆盖只告警，
//!    因为运维可能故意用内部别名做文件名）；
//! 4. Unix 下私钥文件不能对同组 / 其他用户可读（`[tls].strict_key_perms`）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::tls::cert;

/// 单条证书记录。
#[derive(Debug, Clone)]
pub struct CertEntry {
    /// 域名模式（精确域名或 `*.example.com`）；默认证书为空串。
    pub pattern: String,
    /// 证书链 + 私钥。
    pub key: Arc<CertifiedKey>,
    /// 到期时间（Unix 秒，解析失败为 `None`）。
    pub not_after: Option<i64>,
}

/// 证书加载选项（`[tls]` 段透传）。
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions {
    /// 私钥文件权限过宽（同组 / 其他用户可读）时是否拒绝加载；`false` 只告警。
    ///
    /// 仅 Unix 生效：Windows 的 ACL 模型无法用 `mode` 位表达。
    pub strict_key_perms: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            strict_key_perms: true,
        }
    }
}

/// 证书仓库：域名模式 → 证书。
#[derive(Debug, Clone, Default)]
pub struct CertStore {
    entries: Vec<CertEntry>,
    /// 未匹配 SNI（或客户端未发 SNI）时的回退证书。
    default: Option<CertEntry>,
}

impl CertStore {
    /// 由 [`CertProvider`] 加载。
    ///
    /// # Errors
    /// 证书缺失 / 解析失败时返回 `50005`。
    pub async fn new<P: CertProvider>(provider: P) -> AppResult<Self> {
        provider.load().await
    }

    /// 直接从目录加载（启动期同步加载，省掉一次 `await`）。
    ///
    /// 使用 [`LoadOptions::default`]（严格校验私钥权限）。
    ///
    /// # Errors
    /// 目录不存在、无任何证书、PEM 解析失败时返回 `50005`。
    pub fn from_dir(dir: impl AsRef<Path>) -> AppResult<Self> {
        Self::from_dir_with(dir, LoadOptions::default())
    }

    /// 直接从目录加载，并指定加载选项。
    ///
    /// # Errors
    /// 目录不存在、无任何证书、PEM 解析失败、私钥与证书不配对、
    /// 链首是 CA 证书、或私钥权限过宽（`strict_key_perms`）时返回 `50005`。
    pub fn from_dir_with(dir: impl AsRef<Path>, opts: LoadOptions) -> AppResult<Self> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("证书目录不存在：{}", dir.display()),
            ));
        }

        let mut entries = Vec::new();
        let domains_dir = dir.join("domains");
        if domains_dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&domains_dir)
                .map_err(|error| read_error(&domains_dir, error))?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pem"))
                .collect();
            // 稳定顺序：便于日志与测试复现
            files.sort();

            for cert_path in files {
                let Some(stem) = cert_path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let key_path = domains_dir.join(format!("{stem}.key"));
                if !key_path.is_file() {
                    tracing::warn!(
                        cert = %cert_path.display(),
                        "缺少配套私钥（{stem}.key），已跳过"
                    );
                    continue;
                }
                let pattern = stem.to_ascii_lowercase();
                let (key, info) = load_pair(&cert_path, &key_path, opts)?;
                check_leaf(&cert_path, &key, info.as_ref(), Some(&pattern))?;
                entries.push(CertEntry {
                    pattern,
                    key: Arc::new(key),
                    not_after: info.map(|info| info.not_after),
                });
            }
        }

        let default = match (dir.join("fullchain.pem"), dir.join("privkey.pem")) {
            (cert_path, key_path) if cert_path.is_file() && key_path.is_file() => {
                let (key, info) = load_pair(&cert_path, &key_path, opts)?;
                // 默认证书没有 SNI 键，跳过 SAN 一致性检查
                check_leaf(&cert_path, &key, info.as_ref(), None)?;
                Some(CertEntry {
                    pattern: String::new(),
                    key: Arc::new(key),
                    not_after: info.map(|info| info.not_after),
                })
            }
            _ => None,
        };

        if entries.is_empty() && default.is_none() {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!(
                    "{} 下没有可用证书（需 fullchain.pem/privkey.pem 或 domains/*.pem）",
                    dir.display()
                ),
            ));
        }

        Ok(Self { entries, default })
    }

    /// 按 SNI 选证书：精确匹配 → 通配符 → 默认证书。
    ///
    /// 未匹配且无默认证书时返回 `None`，rustls 会**拒绝握手**。
    #[must_use]
    pub fn resolve(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        self.resolve_entry(server_name)
            .map(|entry| Arc::clone(&entry.key))
    }

    /// 按 SNI 选证书记录（需要域名模式时用它，如 OCSP 装订按模式缓存）。
    #[must_use]
    pub fn resolve_entry(&self, server_name: Option<&str>) -> Option<&CertEntry> {
        if let Some(name) = server_name {
            let name = name.to_ascii_lowercase();
            if let Some(entry) = self.entries.iter().find(|entry| entry.pattern == name) {
                return Some(entry);
            }
            if let Some(entry) = self
                .entries
                .iter()
                .find(|entry| wildcard_match(&entry.pattern, &name))
            {
                return Some(entry);
            }
        }
        self.default.as_ref()
    }

    /// 全部证书（含默认证书，默认证书的 `pattern` 为空串）。
    #[must_use]
    pub fn entries(&self) -> Vec<&CertEntry> {
        self.entries.iter().chain(self.default.as_ref()).collect()
    }

    /// 是否为空（无任何证书）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.default.is_none()
    }
}

/// 通配符匹配：`*.example.com` **只匹配一级**子域。
///
/// `a.example.com` ✓；`a.b.example.com` ✗；`example.com` ✗。
#[must_use]
pub fn wildcard_match(pattern: &str, server_name: &str) -> bool {
    let Some(suffix) = pattern.strip_prefix("*.") else {
        return false;
    };
    let suffix = format!(".{}", suffix.to_ascii_lowercase());
    let Some(prefix) = server_name.strip_suffix(&suffix) else {
        return false;
    };
    !prefix.is_empty() && !prefix.contains('.')
}

/// 证书 SAN 是否覆盖某个 SNI 键（`pattern` 即握手时客户端会发的名字）。
///
/// `*.example.com` 的证书能覆盖 `api.example.com`，所以两侧都按通配符语义比。
#[must_use]
pub fn san_covers(sans: &[String], pattern: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    sans.iter().any(|san| {
        let san = san.to_ascii_lowercase();
        san == pattern || wildcard_match(&san, &pattern)
    })
}

/// 证书来源（默认实现为本地文件，预留 DB / KMS / ACME 缓存）。
#[async_trait]
pub trait CertProvider: Send + Sync {
    /// 加载全部证书。
    ///
    /// # Errors
    /// 证书缺失或解析失败时返回错误。
    async fn load(&self) -> AppResult<CertStore>;
}

/// 本地文件证书来源。
#[derive(Debug, Clone)]
pub struct FileCertProvider {
    dir: PathBuf,
}

impl FileCertProvider {
    /// 指定证书目录。
    #[must_use]
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 证书目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[async_trait]
impl CertProvider for FileCertProvider {
    async fn load(&self) -> AppResult<CertStore> {
        CertStore::from_dir(&self.dir)
    }
}

/// 链首必须是叶子证书，且 SAN 应覆盖文件名（SNI 键）。
fn check_leaf(
    cert_path: &Path,
    key: &CertifiedKey,
    info: Option<&cert::CertInfo>,
    pattern: Option<&str>,
) -> AppResult<()> {
    let Some(info) = info else {
        // 解析不了就拿不到到期时间，到期告警会静默失效——必须留痕
        tracing::warn!(
            cert = %cert_path.display(),
            "证书无法解析（x509-parser），到期告警与 SAN 校验对这张证书失效"
        );
        return Ok(());
    };

    if info.is_ca {
        return Err(AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!(
                "{} 的链首是 CA 证书（basicConstraints.cA = true）：\
                 fullchain 必须按「叶子 → 中间 → 根」顺序拼接",
                cert_path.display()
            ),
        ));
    }

    if let Some(pattern) = pattern {
        if info.sans.is_empty() {
            tracing::warn!(
                cert = %cert_path.display(),
                pattern = %pattern,
                "证书没有 SAN 扩展（依赖 CN 的证书已被主流客户端弃用），\
                 客户端可能拒绝该 SNI"
            );
        } else if !san_covers(&info.sans, pattern) {
            tracing::warn!(
                cert = %cert_path.display(),
                pattern = %pattern,
                sans = ?info.sans,
                "证书 SAN 不覆盖文件名（SNI 键）：该 SNI 会拿到一张名字不匹配的证书"
            );
        }
    }

    // 链首必须存在（`load_pair` 已保证非空），这里只是让断言更显式
    debug_assert!(!key.cert.is_empty());
    Ok(())
}

/// 读取并解析一对 PEM（证书链 + 私钥）。
fn load_pair(
    cert_path: &Path,
    key_path: &Path,
    opts: LoadOptions,
) -> AppResult<(CertifiedKey, Option<cert::CertInfo>)> {
    let cert_pem = std::fs::read(cert_path).map_err(|error| read_error(cert_path, error))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|error| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("解析证书失败 {}：{error}", cert_path.display()),
            )
        })?;
    if certs.is_empty() {
        return Err(AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("{} 中没有证书", cert_path.display()),
        ));
    }

    check_key_perms(key_path, opts.strict_key_perms)?;

    let key_pem = std::fs::read(key_path).map_err(|error| read_error(key_path, error))?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|error| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("解析私钥失败 {}：{error}", key_path.display()),
            )
        })?
        .ok_or_else(|| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("{} 中没有私钥", key_path.display()),
            )
        })?;

    // 解析信息要在 `certs` 被移动前取
    let info = cert::parse(certs[0].as_ref());

    // 用当前 CryptoProvider 加载私钥，并**校验私钥与叶子证书配对**
    // （`CertifiedKey::new` 不做这个检查，配错要到握手时才暴露成客户端报错）
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let certified = CertifiedKey::from_der(certs, key, &provider).map_err(|error| {
        let hint = match error {
            rustls::Error::InconsistentKeys(_) => "（私钥与证书不配对）",
            _ => "",
        };
        AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!("私钥不可用 {}{hint}：{error}", key_path.display()),
        )
    })?;

    Ok((certified, info))
}

/// Unix 下检查私钥文件权限：同组 / 其他用户不得可读写。
#[cfg(unix)]
fn check_key_perms(key_path: &Path, strict: bool) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::metadata(key_path).map_err(|error| read_error(key_path, error))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return Ok(());
    }

    if strict {
        return Err(AppError::biz(
            ErrorCode::SERVICE_UNAVAILABLE,
            format!(
                "私钥文件权限过宽 {}：{mode:o}（同组/其他用户可访问，应为 600）。\
                 执行 `chmod 600 <文件>` 后重试，或显式设置 [tls].strict_key_perms = false",
                key_path.display()
            ),
        ));
    }
    tracing::warn!(
        key = %key_path.display(),
        mode = format!("{mode:o}"),
        "私钥文件权限过宽（同组/其他用户可访问），建议 chmod 600"
    );
    Ok(())
}

/// 非 Unix 平台不做权限判定（ACL 无法用 mode 位表达）。
#[cfg(not(unix))]
fn check_key_perms(_key_path: &Path, _strict: bool) -> AppResult<()> {
    Ok(())
}

fn read_error(path: &Path, error: std::io::Error) -> AppError {
    AppError::biz(
        ErrorCode::SERVICE_UNAVAILABLE,
        format!("读取证书文件失败 {}：{error}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_match_wildcard_one_level_only() {
        assert!(wildcard_match("*.example.com", "api.example.com"));
        assert!(!wildcard_match("*.example.com", "a.b.example.com"));
        assert!(!wildcard_match("*.example.com", "example.com"));
        assert!(!wildcard_match("api.example.com", "api.example.com"));
    }

    #[test]
    fn should_match_san_against_pattern() {
        let sans = vec!["api.example.com".to_string(), "*.example.org".to_string()];
        assert!(san_covers(&sans, "api.example.com"));
        assert!(san_covers(&sans, "API.EXAMPLE.COM"), "大小写不敏感");
        assert!(
            san_covers(&sans, "www.example.org"),
            "通配符 SAN 覆盖一级子域"
        );
        assert!(!san_covers(&sans, "a.b.example.org"), "通配符只覆盖一级");
        assert!(!san_covers(&sans, "evil.example.com"));
        assert!(!san_covers(&[], "api.example.com"));
    }

    #[test]
    fn should_reject_missing_dir() {
        let error = CertStore::from_dir("no/such/certs").unwrap_err();
        assert_eq!(error.code(), ErrorCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn should_reject_empty_dir() {
        let dir = std::env::temp_dir().join("aurora-empty-certs");
        std::fs::create_dir_all(&dir).unwrap();
        let error = CertStore::from_dir(&dir).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SERVICE_UNAVAILABLE);
    }
}
