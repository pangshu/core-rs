//! [`TestApp`]：一行起测试应用。
//!
//! 装配链路与生产一致：`AppConfig` → [`AppState::build`] → `build_router`
//! （**含完整中间件栈**与 `/health`、`/ready`），再交给 `axum-test` 的 `TestServer`。
//!
//! 每次测试新建一个 `TestApp` 即为**独立状态**（内存后端天然隔离），见 `docs/14` §3。

use axum::Router;
use axum_test::{TestResponse, TestServer};
use serde::Serialize;

use crate::app::AppState;
use crate::boot::startup::StartupError;
use crate::config::{AppConfig, Environment};
use crate::http::{AppRoutes, build_router};

/// 测试装配器。
pub struct TestApp {
    server: TestServer,
    state: AppState,
}

impl std::fmt::Debug for TestApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestApp").finish_non_exhaustive()
    }
}

impl TestApp {
    /// 用**测试默认配置**装配（无业务路由）。
    ///
    /// 启用 `db` 时自动补 `sqlite::memory:`（`docs/14` §4：DB 测试一律内存库）。
    ///
    /// # Panics
    /// 装配失败时 panic（测试场景下即视为用例失败）。
    pub async fn new() -> Self {
        Self::with_routes(test_config(), Router::new()).await
    }

    /// 用给定配置装配（无业务路由）。
    ///
    /// # Panics
    /// 装配失败时 panic。
    pub async fn with_config(config: AppConfig) -> Self {
        Self::with_routes(config, Router::new()).await
    }

    /// 用给定配置 + 业务路由装配。
    ///
    /// **裸 `Router` 会被当作「全部受保护」**（`From<Router> for AppRoutes` →
    /// `all_protected`）：启用了 `auth` 时这些路由要求 `Identity`，匿名请求得到
    /// `401` / `20001`。要测匿名可访问的路径，请显式传
    /// `AppRoutes::all_public(router)` 或 `AppRoutes::public(|r| ...)`——
    /// 这个默认值是有意的（生产代码里漏标 = 拒绝访问，而不是悄悄放开）。
    ///
    /// 只测中间件 / 端点行为、不关心鉴权时，直接用 [`TestApp::with_public_routes`]。
    ///
    /// # Panics
    /// 装配失败时 panic。
    pub async fn with_routes(config: AppConfig, routes: impl Into<AppRoutes>) -> Self {
        match Self::try_with_routes(config, routes).await {
            Ok(app) => app,
            Err(error) => panic!("TestApp 装配失败：{error}"),
        }
    }

    /// 用给定配置 + **全部匿名可达**的业务路由装配。
    ///
    /// 等价于 `with_routes(config, AppRoutes::all_public(routes))`。
    ///
    /// 只适用于「被测对象与鉴权无关」的用例（指标端点、限流、i18n、实时握手等）。
    /// 生产代码不要这样挂路由：`all_public` 等于放弃「默认拒绝」。
    ///
    /// # Panics
    /// 装配失败时 panic。
    pub async fn with_public_routes(config: AppConfig, routes: Router<AppState>) -> Self {
        Self::with_routes(config, AppRoutes::all_public(routes)).await
    }

    /// 非 panic 版本：装配失败返回 [`StartupError`]。
    ///
    /// # Errors
    /// 配置校验、资源连接或 `TestServer` 构建失败时返回错误。
    pub async fn try_with_routes(
        config: AppConfig,
        routes: impl Into<AppRoutes>,
    ) -> Result<Self, StartupError> {
        // 测试装配同样走启动期校验（生产链路由 `AppConfig::load` 完成）。
        config.validate()?;
        let state = AppState::build(&config).await?;
        let app = with_test_peer(build_router(routes, state.clone()));
        let server =
            TestServer::new(app).map_err(|error| StartupError::Serve(error.to_string()))?;
        Ok(Self { server, state })
    }

    /// 用**HTTP 传输层**装配（WebSocket 端到端测试必需）。
    ///
    /// `TestServer::new` 走 mock 传输层，`into_websocket()` 会直接失败——
    /// 需要真实升级握手时用这个构造函数。
    ///
    /// # Panics
    /// 装配失败时 panic。
    pub async fn with_routes_http(config: AppConfig, routes: impl Into<AppRoutes>) -> Self {
        config.validate().expect("测试配置应通过校验");
        let state = AppState::build(&config).await.expect("构建状态");
        let app = with_test_peer(crate::http::build_router(routes, state.clone()));
        let server = TestServer::builder()
            .http_transport()
            .build(app)
            .expect("构建 HTTP 传输层 TestServer");
        Self { server, state }
    }

    /// 用**HTTP 传输层**装配 + **全部匿名可达**的业务路由。
    ///
    /// 等价于 `with_routes_http(config, AppRoutes::all_public(routes))`。
    /// 实时（WebSocket / SSE）用例走这个：鉴权由 `Hub::on_subscribe` 负责，
    /// 不在路由守卫这一层。
    ///
    /// # Panics
    /// 装配失败时 panic。
    pub async fn with_public_routes_http(config: AppConfig, routes: Router<AppState>) -> Self {
        Self::with_routes_http(config, AppRoutes::all_public(routes)).await
    }

    /// 底层 `TestServer`（需要自定义请求时使用）。
    #[must_use]
    pub fn server(&self) -> &TestServer {
        &self.server
    }

    /// 装配好的应用状态（用于播种数据 / 断言探针）。
    #[must_use]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// `GET` 请求。
    pub async fn get(&self, path: &str) -> TestResponse {
        self.server.get(path).await
    }

    /// `POST` JSON 请求。
    pub async fn post_json<B: Serialize>(&self, path: &str, body: &B) -> TestResponse {
        self.server.post(path).json(body).await
    }

    /// `PUT` JSON 请求。
    pub async fn put_json<B: Serialize>(&self, path: &str, body: &B) -> TestResponse {
        self.server.put(path).json(body).await
    }

    /// `DELETE` 请求。
    pub async fn delete(&self, path: &str) -> TestResponse {
        self.server.delete(path).await
    }
}

/// 给测试请求补一个模拟的 TCP 对端（`127.0.0.1`）。
///
/// `axum_test` 的传输层不产生 `ConnectInfo`，而限流 / IP 黑白名单 / geoip
/// 都依赖它判定「来源」。缺了它，框架会按 fail-closed 处理成「来源不可知」
/// （见 `utils::client_ip`）——这是刻意的：真实部署里拿不到对端地址本身就是
/// 错误配置，不该静默退回「信任客户端自带的 XFF」。
///
/// 测试里补上本机地址，等价于「客户端直连」，各中间件的行为与生产一致。
fn with_test_peer(app: Router) -> Router {
    app.layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    0,
                ))));
            next.run(req).await
        },
    ))
}

/// 测试默认配置。
///
/// 环境固定为 [`Environment::Development`]：`Environment::default()` 是
/// `Production`（刻意的生产护栏默认值），而生产护栏会拒绝「没有 JWT 密钥 /
/// 没有 metrics 令牌」的配置——测试用的空密钥配置在那套规则下**根本起不来**。
/// 这里显式降级，是为了让测试跑的是**业务行为**，而不是生产配置校验。
/// 需要验证生产护栏本身的用例（如 `tests/config_env.rs`），自行构造
/// `Environment::Production` 的配置，不走本函数。
#[must_use]
pub fn test_config() -> AppConfig {
    // 未启用 `db` 时下面的赋值不生效，故允许 `unused_mut`。
    #[allow(unused_mut)]
    let mut config = AppConfig {
        env: Environment::Development,
        ..AppConfig::default()
    };
    // 启用 `db` 时 `database.url` 必填；测试统一用 SQLite 内存库。
    #[cfg(feature = "db")]
    {
        config.database.url = "sqlite::memory:".to_string();
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{AppError, AppResult};
    use crate::http::ApiResponse;
    use axum::routing::get;

    async fn ping() -> AppResult<ApiResponse<&'static str>> {
        Ok(ApiResponse::ok("pong"))
    }

    async fn boom() -> AppResult<ApiResponse<()>> {
        Err(AppError::not_found("nope"))
    }

    #[tokio::test]
    async fn should_serve_health_endpoint() {
        let app = TestApp::new().await;
        app.get("/health").await.assert_status_ok();
    }

    #[tokio::test]
    async fn should_serve_business_route_with_envelope() {
        // 裸 `Router` 走 `From<Router>` → `all_protected`（默认受保护）；
        // 本用例只验响应体封装，不关心鉴权，故走 `with_public_routes`。
        let app =
            TestApp::with_public_routes(test_config(), Router::new().route("/ping", get(ping)))
                .await;

        let response = app.get("/ping").await;
        response.assert_status_ok();
        let body: serde_json::Value = response.json();
        assert_eq!(body["code"], 0);
        assert_eq!(body["data"], "pong");
        assert_eq!(body["trace_id"].as_str().map(str::len), Some(32));
    }

    #[tokio::test]
    async fn should_apply_middleware_stack() {
        let app =
            TestApp::with_public_routes(test_config(), Router::new().route("/boom", get(boom)))
                .await;

        let response = app.get("/boom").await;
        response.assert_status(axum::http::StatusCode::NOT_FOUND);
        let body: serde_json::Value = response.json();
        assert_eq!(body["code"], 30001);
        // 中间件栈生效：应回写 trace-id
        response.assert_header("x-trace-id", body["trace_id"].as_str().unwrap());
    }

    #[tokio::test]
    async fn should_expose_state_for_seeding() {
        let app = TestApp::new().await;
        #[cfg(feature = "cache")]
        {
            app.state().cache.set("k", b"v", None).await.unwrap();
            assert_eq!(
                app.state().cache.get("k").await.unwrap().as_deref(),
                Some(&b"v"[..])
            );
        }
        let _ = app.state();
    }

    #[tokio::test]
    async fn should_return_error_when_config_invalid() {
        let mut config = test_config();
        config.server.port = 0;
        let result = TestApp::try_with_routes(config, Router::new()).await;
        assert!(result.is_err(), "非法配置应返回错误而非 panic");
    }
}
