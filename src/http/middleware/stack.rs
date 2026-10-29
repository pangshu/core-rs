//! 中间件栈统一装配。
//!
//! 权威顺序（由外到内，见 `docs/02` §6）：
//! `panic → request_id → trace → access_log → metrics → security_headers →
//! envelope(413) → body_limit → envelope(408) → timeout → ip_filter → cors →
//! compression → locale → csrf → rate_limit → auth → idempotency`
//!
//! **⚠️ 层序陷阱**：`axum::Router::layer` 是「**后添加=外层**」，与
//! `tower::ServiceBuilder` 的「先添加=外层」**相反**。因此这里按**由内到外**的
//! 逆序逐层 `.layer()`，等价于上表的由外到内顺序；这样每一步的内层服务类型都是
//! 具体的，既避免 `ServiceBuilder` 的泛型地狱，也避免层序被写反。
//!
//! **不可调换的约束**：
//! - `idempotency` 必须在 `auth` **内侧**：命中回放时会 `return` 而不调用
//!   `next.run`，若它在鉴权之外，未认证请求就能读到别人的缓存响应。
//! - `rate_limit` 必须在 `auth` **外侧**：`authenticate` 对无 `Authorization` 的请求
//!   是放行的，限流若在其内侧，匿名流量（暴力破解的主要来源）就完全不受限。
//!   代价是这里拿不到 `Identity`，`per_key` 只能靠**本层自己验签**（见该层注释）。
//! - `ip_filter` 必须在 `cors` **外侧**：`CorsLayer` 会短路 `OPTIONS` 预检、不再调用
//!   `next.run`，若 IP 过滤在其内侧，被拉黑的来源照样能拿到 200 预检响应。
//! - `envelope` 的两层必须分别紧贴 `body_limit` / `timeout` **外侧**：它们要把这两个
//!   层产出的裸 413 / 408 改写成统一响应体。
//!
//! feature 门控层通过 `if` 分支逐层追加（不改变已有层类型）。

#[cfg(feature = "auth")]
use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn;

use crate::app::state::AppState;
use crate::http::middleware::{
    body_limit, compression, cors, envelope, panic, request_id, timeout, trace,
};
use crate::utils::ProxyTrust;

/// 为 Router 套上中间件栈（由内到外逐层装配）。
pub(crate) fn apply(mut router: Router<AppState>, state: &AppState) -> Router<AppState> {
    let config = state.config.load();
    let server = &config.server;

    // —— 由内到外（逆序于权威顺序）——

    // 幂等性（最内，feature idempotency）：同键请求回放首次结果 / 409
    // 必须在鉴权内侧：回放会短路后续所有层（含 auth），只有已注入 Identity
    // 的请求才允许回放。
    #[cfg(feature = "idempotency")]
    {
        let cache = state.cache.clone();
        let idem_config = crate::http::middleware::idempotency::Config::from_section(
            &config.http.idempotency,
            config.server.body_limit_bytes(),
            config.server.request_timeout,
        );
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                crate::http::middleware::idempotency::handle(
                    cache.clone(),
                    idem_config.clone(),
                    req,
                    next,
                )
            },
        ));
    }

    // 鉴权：解析 Bearer 令牌并注入 Identity
    #[cfg(feature = "auth")]
    {
        // `/metrics` 用的是**抓取令牌**（`[telemetry].metrics.token`），不是 JWT；
        // 若让它也过 JWT 校验，Prometheus 发来的 `Authorization: Bearer <token>`
        // 会被这里直接 401，抓取永远失败。该端点自带 Bearer 校验，故按**精确路径**放行。
        // **只放行 GET / HEAD**：`metrics.path` 是配置项，若它恰好等于某个只注册了
        // POST 的业务路径（axum 允许同路径不同方法并存），路径级旁路会让该路径的
        // POST 一起跳过 JWT 校验——那是鉴权绕过，不是便利。
        #[cfg(feature = "metrics")]
        let metrics_path = config
            .telemetry
            .metrics
            .enabled
            .then(|| config.telemetry.metrics.path.clone());
        #[cfg(not(feature = "metrics"))]
        let metrics_path: Option<String> = None;

        router = router.layer(from_fn({
            let auth = Arc::new(state.auth.clone());
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let auth = Arc::clone(&auth);
                let metrics_path = metrics_path.clone();
                async move {
                    // 精确匹配（不是前缀）+ 仅读方法：只放过指标端点本身
                    let is_metrics = metrics_path
                        .as_deref()
                        .is_some_and(|path| req.uri().path() == path);
                    let is_read = *req.method() == axum::http::Method::GET
                        || *req.method() == axum::http::Method::HEAD;
                    if is_metrics && is_read {
                        return next.run(req).await;
                    }
                    crate::auth::authenticate(auth, req, next).await
                }
            }
        }));
    }

    // 限流（feature ratelimit）：超限返回 40001（429）+ Retry-After
    // 必须在鉴权外侧：匿名请求也要计入配额（`authenticate` 对无 token 的请求放行）。
    // 代价是这里拿不到 `Identity`，所以 `per_key` 的维度只能来自**本层自己验签**
    // 的主体（`verified_claims`），并把验签结果放进扩展供内层 `authenticate` 复用。
    // 验签放在 `handle` 内部而不是这里：`enabled = false` 或路径命中豁免清单时
    // 直接短路，不该为一个马上要被跳过的层付一次 RS256。
    #[cfg(feature = "ratelimit")]
    {
        let service = state.rate_limit.clone();
        // 信任策略**每次请求重新读取**（与 `ClientIp` 提取器一致）：收紧
        // `trusted_proxies` 后无需重启即可生效
        let config_handle = state.config.clone();
        #[cfg(feature = "auth")]
        let auth = Arc::new(state.auth.clone());
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let service = service.clone();
                let config_handle = config_handle.clone();
                #[cfg(feature = "auth")]
                let auth = Arc::clone(&auth);
                async move {
                    // 读**冻结的安全边界**：热更新不得放宽信任判定（见 SecurityBoundary）
                    let boundary = config_handle.boundary();
                    let trust = ProxyTrust::new(boundary.trust_proxy, &boundary.trusted_proxies);

                    crate::http::middleware::rate_limit::handle(
                        service,
                        #[cfg(feature = "auth")]
                        auth,
                        trust,
                        req,
                        next,
                    )
                    .await
                }
            },
        ));
    }

    // CSRF（feature csrf）：双提交 Cookie，失败返回 20002（403）
    #[cfg(feature = "csrf")]
    {
        // 启动期构造一次：签名密钥不该在请求路径上反复克隆
        let codec = crate::http::middleware::csrf::CsrfCodec::from_section(&config.http.csrf);
        let enabled = config.http.csrf.enabled;
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let codec = codec.clone();
                async move {
                    if !enabled {
                        return next.run(req).await;
                    }
                    crate::http::middleware::csrf::handle(codec, req, next).await
                }
            },
        ));
    }

    // 语言协商（feature i18n）：协商一次并注入扩展 + Content-Language
    #[cfg(feature = "i18n")]
    {
        let i18n = state.i18n.clone();
        let enabled = config.http.locale.enabled;
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                crate::http::middleware::locale::handle(i18n.clone(), enabled, req, next)
            },
        ));
    }

    if config.http.compression.enabled {
        router = router.layer(compression::layer());
    }

    router = router.layer(cors::layer(&server.cors));

    // IP 黑白名单（feature ip-filter）：受限返回 40002（403）
    // 必须在 `cors` **外侧**：`CorsLayer` 对 `OPTIONS` 预检会短路、不再调用
    // `next.run`，若本层在其内侧，被拉黑的来源仍能拿到 200 预检响应。
    #[cfg(feature = "ip-filter")]
    {
        let rules = config.http.ip_filter.clone();
        let config_handle = state.config.clone();
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let rules = rules.clone();
                let config_handle = config_handle.clone();
                async move {
                    let boundary = config_handle.boundary();
                    let trust = ProxyTrust::new(boundary.trust_proxy, &boundary.trusted_proxies);
                    crate::http::middleware::ip_filter::handle(rules, trust, req, next).await
                }
            },
        ));
    }

    router = router.layer(timeout::layer(server.request_timeout));
    // 裸 408 归一成统一响应体（必须紧贴超时层外侧）
    router = router.layer(from_fn(envelope::normalize_request_timeout));

    router = router.layer(body_limit::layer(server.body_limit_bytes()));
    // 裸 413 归一成统一响应体（必须紧贴 body_limit 外侧）
    router = router.layer(from_fn(envelope::normalize_payload_too_large));

    // 安全响应头（feature security-headers）
    #[cfg(feature = "security-headers")]
    {
        let cfg = config.http.security_headers.clone();
        // 应用内 TLS 终结时无需看 `X-Forwarded-Proto`；前置网关终结时由该头决定
        #[cfg(feature = "tls")]
        let app_tls = config.tls.enabled;
        #[cfg(not(feature = "tls"))]
        let app_tls = false;
        let config_handle = state.config.clone();
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let cfg = cfg.clone();
                let config_handle = config_handle.clone();
                async move {
                    let boundary = config_handle.boundary();
                    let trust = ProxyTrust::new(boundary.trust_proxy, &boundary.trusted_proxies);
                    crate::http::middleware::security_headers::handle(
                        cfg, app_tls, trust, req, next,
                    )
                    .await
                }
            },
        ));
    }

    // 指标（feature metrics）：请求数 / 延迟 / 并发
    #[cfg(feature = "metrics")]
    {
        let metrics = state.metrics.clone();
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                crate::telemetry::metrics::handle(metrics.clone(), req, next)
            },
        ));
    }

    // 访问日志（feature access-log）
    #[cfg(feature = "access-log")]
    {
        let enabled = config.http.access_log.enabled;
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                crate::http::middleware::access_log::handle(enabled, req, next)
            },
        ));
    }

    // 链路追踪
    // 上游 `traceparent` / `X-Trace-Id` 只在对端是**可信代理**时才继承，
    // 否则任意客户端都能指定 trace-id（污染溯源）。信任策略每次请求重新读取。
    {
        let config_handle = state.config.clone();
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let config_handle = config_handle.clone();
                async move {
                    let boundary = config_handle.boundary();
                    let trust = ProxyTrust::new(boundary.trust_proxy, &boundary.trusted_proxies);
                    trace::handle(trust, req, next).await
                }
            },
        ));
    }

    // 请求 ID
    // 客户端自带的 `X-Request-Id` **不采信**（会被回显并写进每行日志，可被用来
    // 伪造日志行 / 混淆调用链）；只有直连对端是可信代理时才继承，与 trace-id 同口径。
    router = router.layer(request_id::propagate_layer());
    {
        let config_handle = state.config.clone();
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let config_handle = config_handle.clone();
                async move {
                    let boundary = config_handle.boundary();
                    let trust = ProxyTrust::new(boundary.trust_proxy, &boundary.trusted_proxies);
                    request_id::handle(trust, req, next).await
                }
            },
        ));
    }

    // panic 兜底（最外）
    router = router.layer(panic::layer());

    router
}
