//! `ClientGeo` 提取器：按客户端 IP 注入地理信息。

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::app::state::AppState;
use crate::geoip::model::GeoInfo;
use crate::utils::{ClientIp, ProxyTrust};

/// 客户端地理信息。
///
/// **四种状态必须显式区分**：此前用 `Option<GeoInfo>`，`None` 同时表示「库没加载」
/// 「内网 IP / 库中无记录」「拿不到客户端 IP」三种完全不同的情况。调用方无法判断
/// 是"确实没有数据"还是"判定不了"，最常见的写法就是
/// `geo.map_or(true, |g| allowed(&g))` —— 库一缺失，风控整体静默放行（fail-open）。
///
/// 现在必须 `match`，把「判定不了」的分支显式写出来。
///
/// ```no_run
/// use aurora::geoip::ClientGeo;
///
/// async fn handler(geo: ClientGeo) -> String {
///     match geo {
///         ClientGeo::Known(info) => info.country_code.unwrap_or_else(|| "unknown".into()),
///         // 库中无记录（内网 / 私有网段 / 未收录）：按"未知地区"处理
///         ClientGeo::NoRecord => "unknown".into(),
///         // 库未加载：**不是"无风险"**，需按策略拒绝或降级到更严格的档位
///         ClientGeo::Unavailable => "unavailable".into(),
///         // 拿不到客户端 IP
///         ClientGeo::UnknownIp => "unknown".into(),
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub enum ClientGeo {
    /// 命中：库已加载，且该 IP 在库中有记录。
    ///
    /// 装箱：`GeoInfo` 约 256 字节，直接内联会让整个枚举在每次请求中按值搬运。
    Known(Box<GeoInfo>),
    /// 库已加载，但该 IP 无记录（内网 / 私有网段 / 库未收录）。
    NoRecord,
    /// 库未加载（降级运行），**无法判定**——与 [`NoRecord`](Self::NoRecord) 不是一回事。
    Unavailable,
    /// 无法确定客户端 IP（无 `ConnectInfo` 且未解析到代理头）。
    UnknownIp,
}

impl ClientGeo {
    /// 命中的地理信息；其余三态均为 `None`。
    ///
    /// 用 `info().is_none()` 做放行判断会退化成 fail-open；
    /// 需要区分原因时请直接 `match`。
    #[must_use]
    pub fn info(&self) -> Option<&GeoInfo> {
        match self {
            Self::Known(info) => Some(info.as_ref()),
            _ => None,
        }
    }

    /// 取走命中的地理信息。
    #[must_use]
    pub fn into_info(self) -> Option<GeoInfo> {
        match self {
            Self::Known(info) => Some(*info),
            _ => None,
        }
    }

    /// 命中的国家代码（ISO 3166-1 alpha-2）。
    #[must_use]
    pub fn country_code(&self) -> Option<&str> {
        self.info().and_then(|info| info.country_code.as_deref())
    }
}

impl FromRequestParts<AppState> for ClientGeo {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 先判库是否加载：部署级故障（路径错 / 镜像漏打包）应报 `Unavailable`，
        // 而不是被后续的 IP 缺失掩盖成 `UnknownIp`
        if !state.geoip.available() {
            return Ok(Self::Unavailable);
        }

        let config = state.config.load();
        let trust = ProxyTrust::new(config.server.trust_proxy, &config.server.trusted_proxies);
        let Some(ip) = ClientIp::from_parts(&parts.headers, &parts.extensions, trust).0 else {
            return Ok(Self::UnknownIp);
        };

        Ok(match state.geoip.lookup(ip) {
            Some(info) => Self::Known(Box::new(info)),
            None => Self::NoRecord,
        })
    }
}
