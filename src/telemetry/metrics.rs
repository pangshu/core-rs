//! 指标（feature `metrics`）。
//!
//! 基于 `prometheus` 的 `Registry` 暴露 HTTP 层指标，并在
//! `[telemetry].metrics.path`（默认 `/metrics`）提供 **Prometheus 文本格式**端点。
//!
//! 指标（`docs/03` §4.5）：
//! - `http_requests_total{method,route,status}` —— 请求计数（错误率 = 非 2xx 占比）
//! - `http_request_duration_seconds{method,route}` —— 延迟直方图（P50/P95/P99 由 Prometheus 计算）
//! - `http_requests_in_flight` —— 并发请求数
//! - `process_start_time_seconds` —— 进程启动时间（用于 uptime）
//! - `validation_rejected_total` —— 输入校验拒绝次数（feature `security`；
//!   与 `http_requests_total{status="400"}` 分开，因为后者把「体解析失败」
//!   和「字段校验失败」混在一起，无法单独告警）
//!
//! **标签基数**：`route` 取 **`MatchedPath`（路由模板）** 而非原始 URI，
//! 否则 `/users/1`、`/users/2`… 会把时间序列打爆；未命中路由记 `unmatched`。
//! `method` 同理只认 9 个标准方法，其余（http crate 接受任意扩展方法）记 `other`。
//!
//! 不引入 `metrics` facade：直接持有 `prometheus` 指标对象，避免全局 recorder
//! 带来的多实例串扰（测试里尤其明显）。

use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};

use crate::app::state::AppState;
use crate::utils::constant_time_eq;

/// 延迟直方图分桶（秒）：覆盖 1ms–10s，够 HTTP 场景用。
const LATENCY_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// 指标集合（`AppState.metrics`）。
///
/// `Clone` 廉价：`Registry` 与各指标内部均为 `Arc`。
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    requests_total: IntCounterVec,
    request_duration: HistogramVec,
    requests_in_flight: IntGauge,
}

impl Metrics {
    /// 创建指标集合并注册到独立 `Registry`。
    ///
    /// 名称与标签固定，注册不会失败；万一失败说明代码写错了（重名 / 非法标签）。
    #[must_use]
    pub fn new() -> Self {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new("http_requests_total", "HTTP 请求总数"),
            &["method", "route", "status"],
        )
        .expect("http_requests_total 定义合法");

        let request_duration = HistogramVec::new(
            HistogramOpts::new("http_request_duration_seconds", "HTTP 请求耗时（秒）")
                .buckets(LATENCY_BUCKETS.to_vec()),
            &["method", "route"],
        )
        .expect("http_request_duration_seconds 定义合法");

        let requests_in_flight = IntGauge::new("http_requests_in_flight", "当前处理中的请求数")
            .expect("http_requests_in_flight 定义合法");

        let start_time =
            prometheus::Gauge::new("process_start_time_seconds", "进程启动时间（Unix 秒）")
                .expect("process_start_time_seconds 定义合法");
        start_time.set(unix_now());

        for collector in [
            Box::new(requests_total.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(request_duration.clone()),
            Box::new(requests_in_flight.clone()),
            Box::new(start_time),
        ] {
            registry
                .register(collector)
                .expect("指标名唯一，注册不会失败");
        }

        // 校验拒绝计数：计数器归 `validation` 模块所有（校验入口拿不到 `AppState`），
        // 这里只把**同一个实例**注册进 Registry，否则自增与导出会落在两个对象上。
        #[cfg(feature = "security")]
        registry
            .register(Box::new(
                crate::validation::metrics::rejected_counter().clone(),
            ))
            .expect("validation_rejected_total 注册不会失败");

        Self {
            registry,
            requests_total,
            request_duration,
            requests_in_flight,
        }
    }

    /// 渲染为 Prometheus 文本格式（`text/plain; version=0.0.4`）。
    #[must_use]
    pub fn render(&self) -> String {
        match TextEncoder::new().encode_to_string(&self.registry.gather()) {
            Ok(body) => body,
            Err(error) => {
                tracing::error!(%error, "指标编码失败");
                String::new()
            }
        }
    }

    /// Prometheus 文本格式的 `Content-Type`。
    #[must_use]
    pub fn content_type() -> &'static str {
        prometheus::TEXT_FORMAT
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics")
            .field("families", &self.registry.gather().len())
            .finish()
    }
}

/// 采集一次请求的指标。
///
/// 放在中间件栈**较外层**（`trace` 内侧），因此耗时包含除 trace / request_id /
/// panic 兜底外的整条链路。
pub(crate) async fn handle(metrics: Metrics, req: Request, next: Next) -> Response {
    let method = method_label(req.method());
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_string(), |path| path.as_str().to_string());

    metrics.requests_in_flight.inc();
    // RAII：本条链路有两条「走不到下一行」的出口——客户端断连（future 被 drop）
    // 与 handler panic（unwind 穿过本层、由最外层 `CatchPanicLayer` 接住）。
    // 用裸 `dec()` 时两者都会漏减，gauge 只增不减 → HPA / 告警永久看到虚高并发。
    let in_flight = InFlightGuard(&metrics.requests_in_flight);
    let started = Instant::now();

    let response = next.run(req).await;
    drop(in_flight);

    let status = response.status().as_u16().to_string();
    metrics
        .requests_total
        .with_label_values(&[method, &route, &status])
        .inc();
    metrics
        .request_duration
        .with_label_values(&[method, &route])
        .observe(started.elapsed().as_secs_f64());

    response
}

/// 在飞请求计数的归还守卫（见 [`handle`] 的注释）。
struct InFlightGuard<'a>(&'a IntGauge);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// `method` 标签的归一化。
///
/// `route` 已经用 `MatchedPath` 归一，`method` 同样不能直出：http crate 接受
/// **任意扩展方法**（`FOO /x` 也是合法请求行），而 404 / 405 同样会经过本层，
/// 于是每个新方法都会新建一组时间序列（counter × status + histogram × 桶）——
/// 未认证即可把内存打爆。白名单外统一记 `other`。
fn method_label(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "CONNECT" => "CONNECT",
        _ => "other",
    }
}

/// `/metrics` 端点（`[telemetry].metrics.enabled = true` 时挂载）。
///
/// 开关、路径与令牌都按**每次抓取的活配置**判定（改令牌 / 关端点都不需要重启）：
/// - `enabled = false` 或路径已改走别处 → `404`。否则「路由在启动期挂死、token 却
///   实时读」的组合会留下一个无鉴权读取口：运维想关指标时通常会把 token 一起清空，
///   而 `token` 只在 `enabled = true` 时才是必填项（校验会放过），端点就裸奔了。
/// - `[telemetry].metrics.token` 非空时要求 `Authorization: Bearer <token>`。
pub(crate) async fn endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    registered_path: String,
) -> Response {
    let config = state.config.load();
    let metrics_cfg = &config.telemetry.metrics;
    if !metrics_cfg.enabled || metrics_cfg.path != registered_path {
        tracing::debug!(
            registered = %registered_path,
            live = %metrics_cfg.path,
            enabled = metrics_cfg.enabled,
            "指标端点已失效（配置热更新）"
        );
        // 该端点输出纯文本、不在统一响应体范围内（`docs/09` §7）
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "not found\n",
        )
            .into_response();
    }

    let expected = metrics_cfg.token.trim();
    if !expected.is_empty() && !bearer_matches(&headers, expected) {
        tracing::debug!("指标端点鉴权失败");
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized\n",
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, Metrics::content_type())],
        state.metrics.render(),
    )
        .into_response()
}

/// 校验 `Authorization: Bearer <token>`。
fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    // 方案名大小写不敏感（RFC 7235 §2.1）
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && token_matches(token.trim(), expected)
}

/// 比较抓取令牌。
///
/// 先各自取 SHA-256 摘要再比：`constant_time_eq` 只在**等长**时是常量时间
/// （长度不等会立即返回，见 `utils::ct` 的边界说明），直接比原文等于把
/// `expected` 的长度送给攻击者。摘要固定 32 字节，长度这一维就被抹掉了。
fn token_matches(presented: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let presented = Sha256::digest(presented.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    constant_time_eq(&presented, &expected)
}
/// 当前 Unix 秒。
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(metrics: Metrics) -> Router {
        Router::new()
            .route("/ping", get(|| async { "pong" }))
            .route("/items/{id}", get(|| async { "item" }))
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let metrics = metrics.clone();
                    async move { handle(metrics, req, next).await }
                },
            ))
    }

    async fn call(router: Router, uri: &str) -> StatusCode {
        router
            .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn should_count_requests_by_route_template() {
        let metrics = Metrics::new();
        let app = app(metrics.clone());

        call(app.clone(), "/ping").await;
        call(app.clone(), "/ping").await;
        call(app.clone(), "/items/1").await;
        call(app.clone(), "/items/2").await;
        call(app, "/nope").await; // 未命中 → unmatched

        let body = metrics.render();
        assert!(
            body.contains(r#"http_requests_total{method="GET",route="/ping",status="200"} 2"#),
            "实际：\n{body}"
        );
        assert!(
            body.contains(
                r#"http_requests_total{method="GET",route="/items/{id}",status="200"} 2"#
            ),
            "路由模板应聚合 /items/1 与 /items/2"
        );
        assert!(
            body.contains(r#"route="unmatched""#),
            "未命中路由不应把原始路径写进标签"
        );
        assert!(!body.contains("/items/1"), "不得出现原始 URI");
    }

    #[tokio::test]
    async fn should_observe_latency_and_in_flight() {
        let metrics = Metrics::new();
        call(app(metrics.clone()), "/ping").await;

        let body = metrics.render();
        assert!(body.contains("http_request_duration_seconds_bucket"));
        assert!(body.contains("http_request_duration_seconds_count"));
        assert!(body.contains("http_requests_in_flight 0"));
    }

    #[test]
    fn should_expose_start_time() {
        let metrics = Metrics::new();
        let body = metrics.render();
        assert!(body.contains("process_start_time_seconds"));
        assert_eq!(Metrics::content_type(), "text/plain; version=0.0.4");
    }
}
