//! TestApp：程序化配置的测试装配 + oneshot 请求助手。

use axum::Router; // 引入 axum 路由类型
use tower::ServiceExt as _; // 以 trait 方式引入 oneshot 调用能力（匿名导入）

use crate::error::AppResult; // 引入框架统一结果别名（用于末尾消警函数）
use crate::state::CoreState; // 引入框架核心状态类型

/// 测试默认配置：sqlite 内存库（feature = "sqlite"）+ memory 缓存/队列 +
/// 安静日志（warn、无 stdout）
pub fn default_test_settings() -> crate::config::Settings { // 构造测试用默认配置
    let mut settings = crate::config::Settings::default(); // 取配置默认值
    settings.database.url = if cfg!(feature = "sqlite") { // 开启 sqlite feature 时用内存库 DSN
        "sqlite::memory:".to_string() // sqlite 内存库连接串
    } else { // 未开启 sqlite 时留空
        String::new() // 空 DSN（由调用方自行设置）
    };
    settings.database.max_connections = 1; // 内存库多连接互不相通
    settings.cache.backend = "memory".to_string(); // 缓存后端用进程内 memory
    settings.queue.backend = "memory".to_string(); // 队列后端用进程内 memory
    settings.log.level = "warn".to_string(); // 日志级别降到 warn，避免测试噪音
    settings.log.stdout = false; // 关闭 stdout 输出
    settings // 返回装配好的配置
}

/// 构建器：允许按用例改配置后再装配
pub struct TestAppBuilder { // 测试应用构建器
    settings: crate::config::Settings, // 待装配的配置
}

impl Default for TestAppBuilder { // 为构建器实现 Default
    fn default() -> Self { // 默认构造委托给 new
        Self::new() // 使用默认测试配置
    }
}

impl TestAppBuilder { // 构建器方法集
    pub fn new() -> Self { // 用默认测试配置构造构建器
        Self { // 组装构建器实例
            settings: default_test_settings(), // 填入默认测试配置
        }
    }

    pub fn new_with(settings: crate::config::Settings) -> Self { // 用指定配置构造构建器
        Self { settings } // 直接使用传入配置
    }

    /// 按用例微调配置（如换 DSN、开关某能力）
    pub fn mutate(mut self, f: impl FnOnce(&mut crate::config::Settings)) -> Self { // 用闭包就地修改配置
        f(&mut self.settings); // 应用调用方提供的修改
        self // 返回自身以支持链式调用
    }

    /// 装配完整 CoreState（配置 → 日志 → db → 缓存 → 队列 → 认证，不拉 watcher）
    pub async fn build<S>(self, make_state: impl FnOnce(CoreState) -> S) -> TestApp<S> // 异步装配 CoreState 并包装成应用状态
    where // 约束泛型应用状态
        S: Clone // 状态需可克隆（每请求克隆注入）
            + Send // 可跨线程发送
            + Sync // 可跨线程共享引用
            + 'static // 不含非静态借用
            + crate::traits::HasDb // 提供数据库连接
            + crate::traits::HasCache // 提供缓存与锁
            + crate::traits::HasQueue // 提供队列
            + crate::traits::HasConfig // 提供配置句柄
            + crate::traits::HasHealthChecks, // 提供健康探针
    {
        let settings = self.settings; // 取出待装配配置
        let core = CoreState::from_settings(settings, crate::config::Environment::Testing) // 以 testing 环境装配核心状态
            .await // 等待异步装配完成
            .expect("TestApp core bootstrap failed"); // 装配失败直接 panic
        let state = make_state(core.clone()); // 用调用方闭包构造应用状态
        TestApp { // 组装测试应用
            state, // 应用状态
            core, // 核心状态
            service: Router::new(), // 初始为空路由（后续 mount 叠加）
        }
    }
}

/// 可直接发请求的测试应用
pub struct TestApp<S> { // 测试应用容器
    pub state: S, // 应用状态（公开便于用例访问）
    pub core: CoreState, // 核心状态（公开便于用例访问）
    service: Router, // 已装配的路由服务
}

impl<S> TestApp<S> // 测试应用方法集
where // 约束泛型应用状态（与 build 一致）
    S: Clone // 状态需可克隆
        + Send // 可跨线程发送
        + Sync // 可跨线程共享引用
        + 'static // 不含非静态借用
        + crate::traits::HasDb // 提供数据库连接
        + crate::traits::HasCache // 提供缓存与锁
        + crate::traits::HasQueue // 提供队列
        + crate::traits::HasConfig // 提供配置句柄
        + crate::traits::HasHealthChecks, // 提供健康探针
{
    /// 挂载应用路由树（可多次调用叠加；自动带 health/ready 与中间件栈，
    /// 与 `App::serve` 默认模式保持一致：panic 兜底 / request_id / locale /
    /// trace / 超时，以及 ip_filter / rate_limit / csrf / idempotency 状态件，
    /// 最外圈注入 `Extension(CoreState)` 作为状态件依赖锚点。
    /// 仅 auth 例外：TestApp 的 S 上界不含 HasAuth，需要认证的用例自行挂）
    pub fn mount(mut self, router: Router<S>) -> Self { // 把业务路由并入已装配服务
        let health = crate::observability::health::routes::<S>(); // 构造 health/ready 探针路由
        let settings = self.core.config.load(); // 读取当前生效配置
        // 状态件与 App::serve 同序（内 → 外）：idempotency → csrf → rate_limit → ip_filter
        let layered = router // 在业务路由上逐层叠加中间件
            .merge(health) // 合并健康探针路由
            .layer(crate::middleware::idempotency::layer()); // 幂等处理层
        #[cfg(feature = "csrf")] // 仅在开启 csrf feature 时编译
        let layered = layered.layer(crate::middleware::csrf::layer()); // CSRF 处理层
        #[cfg(feature = "rate-limit")] // 仅在开启 rate-limit feature 时编译
        let layered = layered.layer(crate::middleware::rate_limit::layer()); // 限流处理层
        let layered = layered.layer(crate::middleware::ip_filter::layer()); // IP 过滤处理层
        let layered = crate::web::router::assemble_base( // 套用与 App::serve 一致的基础栈（panic 兜底/request_id/locale/trace/超时等）
            layered, // 已叠加中间件的路由
            &settings.server, // 服务器配置
            settings.service_name(), // 服务名（用于 trace）
        );
        // CoreState 注入最外圈（状态件依赖锚点，与 App::serve 一致）
        let layered = layered.layer(axum::Extension(self.core.clone())); // 挂载核心状态扩展
        self.service = self.service.merge(layered.with_state(self.state.clone())); // 把新路由并入已装配服务
        self // 返回自身以支持链式调用
    }

    /// 发送原生请求
    pub async fn request( // 以 oneshot 方式发送一个请求
        &self, // 借用自身（service 内部会 clone）
        req: axum::http::Request<axum::body::Body>, // 待发送的原始 HTTP 请求
    ) -> axum::response::Response { // 返回响应
        self.service // 取已装配服务
            .clone() // clone 出可消费的实例（oneshot 消耗所有权）
            .oneshot(req) // 单次调用服务
            .await // 等待响应
            .expect("test request failed") // 调用失败直接 panic
    }

    pub async fn get(&self, uri: &str) -> axum::response::Response { // 便捷发送 GET 请求
        self.request(axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap()) // 构造空体 GET 请求
            .await // 等待响应
    }

    pub async fn post_json( // 便捷发送 JSON POST 请求
        &self, // 借用自身
        uri: &str, // 请求路径
        body: &impl serde::Serialize, // 可序列化的请求体
    ) -> axum::response::Response { // 返回响应
        let json = serde_json::to_vec(body).expect("serialize json body"); // 把请求体序列化为 JSON 字节
        self.request( // 发送请求
            axum::http::Request::builder() // 开始构造请求
                .method(axum::http::Method::POST) // 方法为 POST
                .uri(uri) // 设置路径
                .header(axum::http::header::CONTENT_TYPE, "application/json") // 设置 Content-Type 为 JSON
                .body(axum::body::Body::from(json)) // 设置 JSON 请求体
                .unwrap(), // 构造失败直接 panic
        )
        .await // 等待响应
    }

    /// 读取响应体（UTF-8 JSON 文本）
    pub async fn body_text(res: axum::response::Response) -> String { // 把响应体读成文本
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX) // 读取完整响应体字节（无上限）
            .await // 等待读取完成
            .expect("read body"); // 读取失败直接 panic
        String::from_utf8_lossy(&bytes).to_string() // 以 UTF-8 有损转换返回文本
    }

    /// 读取响应体并反序列化为 ApiResponse JSON
    pub async fn body_json<T: serde::de::DeserializeOwned>( // 把响应体反序列化为 ApiResponse<T>
        res: axum::response::Response, // 待解析的响应
    ) -> crate::web::response::ApiResponse<T> { // 返回统一响应结构
        let text = Self::body_text(res).await; // 先读出响应文本
        serde_json::from_str(&text).unwrap_or_else(|e| { // 解析 JSON，失败则带上下文 panic
            panic!("response is not ApiResponse JSON: {e}; body: {text}") // 报出错误与原始响应体
        })
    }

    /// 兼容 just-in-time 改配置后的重新读取（返回当前生效配置）
    pub fn settings(&self) -> std::sync::Arc<crate::config::Settings> { // 取当前生效配置快照
        self.core.config.load_full() // 无锁加载配置句柄的完整快照
    }
}

// AppResult 导入消警（公开 API 生态一致性）
#[allow(unused)] // 允许该函数未被使用（仅为保持导入）
fn _assert() -> AppResult<()> { // 占位函数，确保 AppResult 导入被使用
    Ok(()) // 返回空成功
}
