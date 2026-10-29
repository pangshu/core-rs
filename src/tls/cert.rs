//! 证书解析：到期时间、SAN 与 CA 标记。
//!
//! 用 `x509-parser` 读取 `notAfter` / `subjectAltName` / `basicConstraints`；
//! 到期阈值由 `[tls].expiry_warn_days` 决定。

/// 框架关心的证书字段（`DER` 解析失败时整个 `parse` 返回 `None`）。
#[derive(Debug, Clone, Default)]
pub struct CertInfo {
    /// `notAfter`（Unix 秒）。
    pub not_after: i64,
    /// SAN 里的 DNS 名（已转小写；没有 SAN 扩展时为空）。
    pub sans: Vec<String>,
    /// `basicConstraints.cA`（没有该扩展时视为 `false`）。
    pub is_ca: bool,
}

/// 从 DER 编码证书解析框架关心的字段；解析失败返回 `None`。
#[must_use]
pub fn parse(der: &[u8]) -> Option<CertInfo> {
    use x509_parser::prelude::FromDer as _;

    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(der).ok()?;

    let mut sans = Vec::new();
    if let Ok(Some(extension)) = parsed.subject_alternative_name() {
        for name in &extension.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(dns) = name {
                sans.push(dns.to_ascii_lowercase());
            }
        }
    }

    let is_ca = parsed
        .basic_constraints()
        .ok()
        .flatten()
        .is_some_and(|constraints| constraints.value.ca);

    Some(CertInfo {
        not_after: parsed.validity().not_after.timestamp(),
        sans,
        is_ca,
    })
}

/// 从 DER 编码证书解析到期时间（Unix 秒）；解析失败返回 `None`。
#[must_use]
pub fn not_after(der: &[u8]) -> Option<i64> {
    parse(der).map(|info| info.not_after)
}

/// 距到期的天数（向下取整；已过期返回负数）。
#[must_use]
pub fn days_until(not_after: i64, now: i64) -> i64 {
    (not_after - now) / 86_400
}

/// 当前 Unix 秒。
#[must_use]
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// 是否已进入告警窗口（`not_after` 未知时不算）。
#[must_use]
pub fn should_warn(not_after: Option<i64>, warn_days: u32, now: i64) -> bool {
    not_after.is_some_and(|not_after| days_until(not_after, now) < i64::from(warn_days))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compute_days_until_expiry() {
        let now = 1_700_000_000;
        assert_eq!(days_until(now + 86_400 * 10, now), 10);
        assert_eq!(days_until(now - 86_400, now), -1);
    }

    #[test]
    fn should_warn_inside_window() {
        let now = 1_700_000_000;
        assert!(should_warn(Some(now + 86_400 * 5), 30, now));
        assert!(!should_warn(Some(now + 86_400 * 40), 30, now));
        assert!(!should_warn(None, 30, now), "到期时间未知时无法判定");
    }

    #[test]
    fn should_return_none_on_garbage() {
        assert!(parse(b"not a certificate").is_none());
        assert!(not_after(&[]).is_none());
    }
}
