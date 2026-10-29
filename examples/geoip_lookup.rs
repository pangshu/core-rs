//! geoip_lookup —— M5 示例：IP 地理定位（MaxMind MMDB）。
//!
//! 运行：`cargo run --example geoip_lookup --features geoip`
//!
//! 说明：GeoLite2 需自行从 MaxMind 下载后放到 `[geoip].city_db`
//! （默认 `data/GeoLite2-City.mmdb`）。**文件缺失时本示例仍可运行**——
//! geoip 会降级为「查询恒为 `None`」，并打印提示。
//!
//! 在 HTTP handler 里用 `ClientGeo` 提取器即可按客户端 IP 取到地理信息。
//! 提取器返回**四态**枚举，必须 `match`（不能把「库未加载」当成「无风险」）：
//! ```ignore
//! async fn handler(geo: ClientGeo) -> String {
//!     match geo {
//!         ClientGeo::Known(info) => info.country_code.unwrap_or_default(),
//!         ClientGeo::NoRecord => "no-record".into(),
//!         ClientGeo::Unavailable => "unavailable".into(),
//!         ClientGeo::UnknownIp => "unknown-ip".into(),
//!     }
//! }
//! ```

use aurora::app::AppState;
use aurora::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = preset::load_env_var("AURORA_ENV", "config")?;
    init_tracing(&config.telemetry)?;
    let state = AppState::build(&config).await?;

    if state.geoip.available() {
        tracing::info!(db = %config.geoip.city_db, "GeoIP 城市库已加载");
    } else {
        tracing::warn!(
            db = %config.geoip.city_db,
            "GeoIP 城市库未加载（降级模式）：下载 GeoLite2-City.mmdb 放到该路径即可生效"
        );
    }

    for raw in ["8.8.8.8", "1.1.1.1", "223.5.5.5", "127.0.0.1"] {
        let ip = raw.parse()?;
        match state.geoip.lookup(ip) {
            Some(info) => tracing::info!(
                %ip,
                country = ?info.country,
                city = ?info.city,
                timezone = ?info.timezone,
                "命中"
            ),
            None => tracing::info!(%ip, "无记录（内网 IP / 库未加载 / 库中无该网段）"),
        }
    }

    Ok(())
}
