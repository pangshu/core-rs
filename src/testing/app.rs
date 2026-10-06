//! TestApp：程序化配置的测试装配 + oneshot 请求助手。

use axum::Router;
use tower::ServiceExt as _;

use crate::error::AppResult;
use crate::state::CoreState;

/// 测试默认配置：sqlite 内存库（feature = "sqlite"）+ memory 缓存/队列 +
/// 安静日志（warn、无 stdout）
pub fn default_test_settings() -> crate::config::Settings {
    let mut settings = crate::config::Settings::default();
    settings.database.url = if cfg!(feature = "sqlite") {
        "sqlite::memory:".to_string()
    } else {
        String::new()
    };
    settings.database.max_connections = 1; // 内存库多连接互不相通
    settings.cache.backend = "memory".to_string();
    settings.queue.backend = "memory".to_string();
    settings.log.level = "warn".to_string();
    settings.log.stdout = false;
    settings
}

/// 构建器：允许按用例改配置后再装配
pub struct TestAppBuilder {
    settings: crate::config::Settings,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestAppBuilder {
    pub fn new() -> Self {
        Self {
            settings: default_test_settings(),
        }
    }

    pub fn new_with(settings: crate::config::Settings) -> Self {
        Self { settings }
    }

    /// 按用例微调配置（如换 DSN、开关某能力）
    pub fn mutate(mut self, f: impl FnOnce(&mut crate::config::Settings)) -> Self {
        f(&mut self.settings);
        self
    }

    /// 装配完整 CoreState（配置 → 日志 → db → 缓存 → 队列 → 认证，不拉 watcher）
    pub async fn build<S>(self, make_state: impl FnOnce(CoreState) -> S) -> TestApp<S>
    where
        S: Clone
            + Send
            + Sync
            + 'static
            + crate::traits::HasDb
            + crate::traits::HasCache
            + crate::traits::HasQueue
            + crate::traits::HasConfig
            + crate::traits::HasHealthChecks,
    {
        let settings = self.settings;
        let core = CoreState::from_settings(settings, crate::config::Environment::Testing)
            .await
            .expect("TestApp core bootstrap failed");
        let state = make_state(core.clone());
        TestApp {
            state,
            core,
            service: Router::new(),
        }
    }
}

/// 可直接发请求的测试应用
pub struct TestApp<S> {
    pub state: S,
    pub core: CoreState,
    service: Router,
}

impl<S> TestApp<S>
where
    S: Clone
        + Send
        + Sync
        + 'static
        + crate::traits::HasDb
        + crate::traits::HasCache
        + crate::traits::HasQueue
        + crate::traits::HasConfig
        + crate::traits::HasHealthChecks,
{
    /// 挂载应用路由树（可多次调用叠加；自动带 health/ready 与中间件栈，
    /// 与 `App::serve` 保持一致：panic 兜底 / request_id / locale / trace / 超时，
    /// 以及 ip_filter / rate_limit / csrf / idempotency 状态件。
    /// 仅 auth 例外：TestApp 的 S 上界不含 HasAuth，需要认证的用例自行挂）
    pub fn mount(mut self, router: Router<S>) -> Self {
        let health = crate::observability::health::routes::<S>();
        let settings = self.core.config.load();
        let layered = router
            .merge(health)
            .layer(axum::middleware::from_fn(
                crate::middleware::request_id::handle,
            ));
        // 状态件与 App::serve 同序（内 → 外）：idempotency → csrf → rate_limit → ip_filter
        let layered = layered.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::idempotency::handle::<S>,
        ));
        #[cfg(feature = "csrf")]
        let layered = layered.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::csrf::handle::<S>,
        ));
        #[cfg(feature = "rate-limit")]
        let layered = layered.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::rate_limit::handle::<S>,
        ));
        let layered = layered.layer(axum::middleware::from_fn_with_state(
            self.state.clone(),
            crate::middleware::ip_filter::handle::<S>,
        ));
        let layered = crate::web::router::assemble_base(
            layered,
            self.state.clone(),
            &settings.server,
            settings.service_name(),
        );
        self.service = self.service.merge(layered.with_state(self.state.clone()));
        self
    }

    /// 发送原生请求
    pub async fn request(
        &self,
        req: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response {
        self.service
            .clone()
            .oneshot(req)
            .await
            .expect("test request failed")
    }

    pub async fn get(&self, uri: &str) -> axum::response::Response {
        self.request(axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap())
            .await
    }

    pub async fn post_json(
        &self,
        uri: &str,
        body: &impl serde::Serialize,
    ) -> axum::response::Response {
        let json = serde_json::to_vec(body).expect("serialize json body");
        self.request(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri(uri)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(json))
                .unwrap(),
        )
        .await
    }

    /// 读取响应体（UTF-8 JSON 文本）
    pub async fn body_text(res: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// 读取响应体并反序列化为 ApiResponse JSON
    pub async fn body_json<T: serde::de::DeserializeOwned>(
        res: axum::response::Response,
    ) -> crate::web::response::ApiResponse<T> {
        let text = Self::body_text(res).await;
        serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!("response is not ApiResponse JSON: {e}; body: {text}")
        })
    }

    /// 兼容 just-in-time 改配置后的重新读取（返回当前生效配置）
    pub fn settings(&self) -> std::sync::Arc<crate::config::Settings> {
        self.core.config.load_full()
    }
}

// AppResult 导入消警（公开 API 生态一致性）
#[allow(unused)]
fn _assert() -> AppResult<()> {
    Ok(())
}
