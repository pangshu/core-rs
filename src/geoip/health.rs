//! 地理库探针（`/ready`）。

use async_trait::async_trait;

use crate::geoip::lookup::GeoIp;
use crate::telemetry::HealthCheck;

/// GeoIP 城市库探针。
///
/// **只在「缺库算不就绪」时才判失败**（`required`）。缺库本身不会让进程不可用
/// ——查询恒为 `None`，业务仍可对外服务；把它一律判成 503 会让开发 / 测试环境
/// 以及显式接受降级的部署被负载均衡反复摘除。
///
/// `required` 的取值即「这份部署是否声明地理信息是必需的」：
/// `production && ![geoip].allow_degraded`。此时缺库说明部署与配置意图矛盾，
/// 应被摘掉，而不是继续拿"地理信息恒为空"的风控对外服务。
///
/// 与启动期校验（[`GeoIpSection::validate_for_env`]）互为双保险：后者挡在配置
/// 加载阶段，本探针覆盖「绕过 `AppConfig::validate()` 直接 `AppState::build`」的路径。
///
/// [`GeoIpSection::validate_for_env`]: crate::config::GeoIpSection::validate_for_env
pub struct GeoIpHealth {
    geoip: GeoIp,
    required: bool,
    name: String,
}

impl GeoIpHealth {
    /// 以地理定位服务与「缺库是否算不就绪」创建探针。
    #[must_use]
    pub fn new(geoip: GeoIp, required: bool) -> Self {
        Self {
            geoip,
            required,
            name: "geoip".to_string(),
        }
    }
}

#[async_trait]
impl HealthCheck for GeoIpHealth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn check(&self) -> Result<(), String> {
        if self.geoip.available() || !self.required {
            return Ok(());
        }
        Err("city database not loaded".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GeoIpSection;

    fn unavailable() -> GeoIp {
        GeoIp::from_config(&GeoIpSection {
            city_db: "data/definitely-not-here.mmdb".to_string(),
            ..GeoIpSection::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn should_fail_when_db_missing_and_required() {
        let probe = GeoIpHealth::new(unavailable(), true);
        // trait 层返回原始原因（只用于日志）；对外文案由注册表统一替换成
        // `geoip unavailable`（见 `telemetry::health` 的边界清洗）
        let error = probe.check().await.unwrap_err();
        assert!(error.contains("city database"), "实际：{error}");
    }

    #[tokio::test]
    async fn should_pass_when_db_missing_but_not_required() {
        // 开发 / 测试环境、或显式 allow_degraded：缺库不应让 /ready 失败
        let probe = GeoIpHealth::new(unavailable(), false);
        assert!(probe.check().await.is_ok());
    }

    #[test]
    fn should_report_probe_name() {
        assert_eq!(GeoIpHealth::new(unavailable(), true).name(), "geoip");
    }
}
