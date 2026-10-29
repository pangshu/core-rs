//! MMDB 读取（`maxminddb`）。
//!
//! 城市库必选（`[geoip].city_db`），ASN 库可选（`[geoip].asn_db`）；
//! 城市库里通常已带 ASN 字段，此时不再查 ASN 库。

use std::net::IpAddr;
use std::path::Path;

use maxminddb::Reader;
use maxminddb::geoip2;

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::geoip::model::{GeoInfo, pick_name};

/// MMDB 读取器（城市库 + 可选 ASN 库）。
pub struct GeoReader {
    city: Reader<Vec<u8>>,
    asn: Option<Reader<Vec<u8>>>,
}

impl std::fmt::Debug for GeoReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoReader")
            .field("asn", &self.asn.is_some())
            .finish_non_exhaustive()
    }
}

impl GeoReader {
    /// 打开数据库。
    ///
    /// # Errors
    /// 城市库不存在或格式非法 → `50005`（启动期暴露）。
    pub fn open(city_db: &Path, asn_db: Option<&Path>) -> AppResult<Self> {
        let city = Reader::open_readfile(city_db).map_err(|error| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("打开 GeoIP 城市库失败 {}: {error}", city_db.display()),
            )
        })?;

        let asn = match asn_db.filter(|path| path.is_file()) {
            Some(path) => Some(Reader::open_readfile(path).map_err(|error| {
                AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    format!("打开 GeoIP ASN 库失败 {}: {error}", path.display()),
                )
            })?),
            None => None,
        };

        Ok(Self { city, asn })
    }

    /// 查询 IP；库中无记录或记录不可解析时返回 `None`。
    #[must_use]
    pub fn lookup(&self, ip: IpAddr) -> Option<GeoInfo> {
        let city: geoip2::City<'_> = match self.city.lookup(ip) {
            Ok(city) => city,
            Err(error) => {
                // `AddressNotFoundError` 是正常情况（内网 IP 等），只记 trace。
                tracing::trace!(%ip, %error, "GeoIP 未命中");
                return None;
            }
        };

        let mut info = GeoInfo {
            ip: Some(ip),
            country: city
                .country
                .as_ref()
                .and_then(|country| country.names.as_ref())
                .and_then(pick_name),
            country_code: city
                .country
                .as_ref()
                .and_then(|country| country.iso_code)
                .map(str::to_string),
            continent_code: city
                .continent
                .as_ref()
                .and_then(|continent| continent.code)
                .map(str::to_string),
            region: city
                .subdivisions
                .as_ref()
                .and_then(|subdivisions| subdivisions.first())
                .and_then(|subdivision| subdivision.names.as_ref())
                .and_then(pick_name),
            city: city
                .city
                .as_ref()
                .and_then(|city| city.names.as_ref())
                .and_then(pick_name),
            postal_code: city
                .postal
                .as_ref()
                .and_then(|postal| postal.code)
                .map(str::to_string),
            latitude: city
                .location
                .as_ref()
                .and_then(|location| location.latitude),
            longitude: city
                .location
                .as_ref()
                .and_then(|location| location.longitude),
            timezone: city
                .location
                .as_ref()
                .and_then(|location| location.time_zone)
                .map(str::to_string),
            // 注：`maxminddb` 的 `City.traits` 复用 `country::Traits`（不含 ASN 字段），
            // 故 ASN 只能来自独立的 ASN 库。
            asn: None,
            organization: None,
        };

        if let Some(asn_reader) = &self.asn
            && let Ok(asn) = asn_reader.lookup::<geoip2::Asn<'_>>(ip)
        {
            info.asn = asn.autonomous_system_number;
            info.organization = asn
                .autonomous_system_organization
                .map(str::to_string)
                .or(info.organization);
        }

        Some(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_fail_on_missing_city_db() {
        let error = GeoReader::open(Path::new("data/not-here.mmdb"), None).unwrap_err();
        assert!(
            error.to_string().contains("GeoIP 城市库"),
            "错误信息应指明城市库，实际：{error}"
        );
    }
}
