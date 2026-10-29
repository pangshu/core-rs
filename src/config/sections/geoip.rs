//! `[geoip]` 配置段（feature `geoip`）。
//!
//! 字段与默认值见 `docs/08-config-spec.md` §3 `[geoip]`。

use std::path::Path;

use serde::Deserialize;

/// 查询缓存条目上限。
///
/// 上限存在的意义是**防误配**：`cache_size` 直接决定进程内存占用（每条目约
/// 300 字节，含多个 `String`），写成 `1e9` 会在流量上来后 OOM。默认值
/// `10_000` 的 10 倍足以覆盖绝大多数单实例场景。
pub const MAX_CACHE_SIZE: usize = 100_000;

/// 地理定位配置段。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct GeoIpSection {
    /// 城市库路径（MMDB）。不存在时**降级运行**（查询恒为 `None`）。
    pub city_db: String,
    /// ASN 库路径（可选；城市库自带 ASN 时无需配置）。
    pub asn_db: Option<String>,
    /// 查询缓存条目上限（`0` = 关闭缓存）。
    pub cache_size: usize,
    /// 生产环境是否允许**缺库降级**（显式确认）。
    ///
    /// 默认 `false`：生产环境 `city_db` 不存在会**拒绝启动**——否则地理信息恒为
    /// 空，依赖它的地区风控会静默失效，而日志里只有一行 `warn`。
    /// 置 `true` 表示明确接受降级（`/ready` 也不会因此失败）。
    pub allow_degraded: bool,
}

impl Default for GeoIpSection {
    fn default() -> Self {
        Self {
            city_db: "data/GeoLite2-City.mmdb".to_string(),
            asn_db: None,
            cache_size: 10_000,
            allow_degraded: false,
        }
    }
}

impl GeoIpSection {
    /// 启动期校验（按**非生产**环境处理，等价于 `validate_for_env(false)`）。
    ///
    /// # Errors
    /// `city_db` 为空、`cache_size` 越界时返回错误。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_for_env(false)
    }

    /// 按运行环境校验（fail-fast）。
    ///
    /// **不校验文件是否存在的场合**：非生产环境缺库是合法的降级场景（便于未持有
    /// MaxMind 许可证的开发环境直接跑起来）。生产环境则相反——缺库等于风控静默
    /// 失效，必须显式 `allow_degraded = true` 才放行。
    ///
    /// # Errors
    /// 路径为空、缓存越界，或生产环境缺库且未显式接受降级时返回错误。
    pub fn validate_for_env(&self, production: bool) -> Result<(), String> {
        if self.city_db.trim().is_empty() {
            return Err("geoip.city_db 不能为空（缺库请保留默认路径并放置文件）".to_string());
        }
        if self.cache_size > MAX_CACHE_SIZE {
            return Err(format!(
                "[geoip].cache_size = {} 超过上限 {MAX_CACHE_SIZE}\
                 （每条目约 300 字节，过大会在流量上来后 OOM）",
                self.cache_size
            ));
        }
        if production && !self.allow_degraded && !Path::new(&self.city_db).is_file() {
            return Err(format!(
                "[geoip].city_db = \"{}\" 不存在：生产环境缺库会让地理信息恒为空，\
                 依赖它的地区风控会静默失效。请放置 GeoLite2-City.mmdb，\
                 或显式设 [geoip].allow_degraded = true（表示接受降级）",
                self.city_db
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_documented_values() {
        let s = GeoIpSection::default();
        assert_eq!(s.city_db, "data/GeoLite2-City.mmdb");
        assert!(s.asn_db.is_none());
        assert_eq!(s.cache_size, 10_000);
        assert!(!s.allow_degraded, "默认不接受降级：生产缺库应拒绝启动");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn should_reject_empty_city_db() {
        let s = GeoIpSection {
            city_db: "  ".to_string(),
            ..GeoIpSection::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_reject_cache_size_above_limit() {
        let s = GeoIpSection {
            cache_size: MAX_CACHE_SIZE + 1,
            ..GeoIpSection::default()
        };
        let error = s.validate().unwrap_err();
        assert!(error.contains("cache_size"), "实际：{error}");

        // 边界值本身合法
        let edge = GeoIpSection {
            cache_size: MAX_CACHE_SIZE,
            ..GeoIpSection::default()
        };
        assert!(edge.validate().is_ok());
    }

    #[test]
    fn should_allow_missing_db_in_non_production() {
        // 开发 / 测试环境缺库是合法的降级场景
        let s = GeoIpSection {
            city_db: "data/definitely-not-here.mmdb".to_string(),
            ..GeoIpSection::default()
        };
        assert!(s.validate_for_env(false).is_ok());
    }

    #[test]
    fn should_reject_missing_db_in_production() {
        let s = GeoIpSection {
            city_db: "data/definitely-not-here.mmdb".to_string(),
            ..GeoIpSection::default()
        };
        let error = s.validate_for_env(true).unwrap_err();
        assert!(error.contains("allow_degraded"), "实际：{error}");
    }

    #[test]
    fn should_allow_missing_db_in_production_when_explicitly_degraded() {
        let s = GeoIpSection {
            city_db: "data/definitely-not-here.mmdb".to_string(),
            allow_degraded: true,
            ..GeoIpSection::default()
        };
        assert!(s.validate_for_env(true).is_ok(), "显式确认后应放行");
    }

    #[test]
    fn should_parse_overrides() {
        let json = r#"{ "city_db": "/var/lib/geo/city.mmdb", "asn_db": "/var/lib/geo/asn.mmdb", "cache_size": 0, "allow_degraded": true }"#;
        let s: GeoIpSection = serde_json::from_str(json).unwrap();
        assert_eq!(s.city_db, "/var/lib/geo/city.mmdb");
        assert_eq!(s.asn_db.as_deref(), Some("/var/lib/geo/asn.mmdb"));
        assert_eq!(s.cache_size, 0);
        assert!(s.allow_degraded);
    }
}
