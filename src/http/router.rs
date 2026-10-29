//! Router 构建与兜底。
//!
//! [`build_router`] 合并业务路由与健康检查，注册 404 / 405 兜底，并套上
//! 中间件栈，最终返回状态已注入的 `Router<()>`。
//!
//! # 默认受保护（fail-closed）
//!
//! 业务路由用 [`AppRoutes`] 声明，**默认进入受保护组**：没有有效 `Identity`
//! 一律 `20001`（401）。公开路由必须显式写进 [`AppRoutes::public`]，
//! 评审时一眼可见"这个接口为什么不需要登录"。
//!
//! 这与"鉴权靠 handler 签名里的 `Principal` 参数"是**互补**关系：
//! - 守卫层（本文件）：粗粒度，保证"漏写参数"不会变成公开接口；
//! - `Principal` 提取器：细粒度，handler 内要拿主体信息时使用。
//!
//! 旧写法 `build_router(Router::new().route(...), state)` 仍然可用（`Router`
//! 可 `Into<AppRoutes>`），但它等价于 [`AppRoutes::all_protected`]——
//! 也就是说**从"默认公开"变成了"默认受保护"**，这是刻意的破坏性变更。

use axum::Router;
#[cfg(feature = "metrics")]
use axum::extract::State;
#[cfg(feature = "metrics")]
use axum::http::HeaderMap;

use crate::app::state::AppState;
use crate::errors::{AppError, ErrorCode};
use crate::http::middleware::stack;

/// 带状态的 axum `Router` 别名。
pub type AppRouter<S = ()> = Router<S>;

/// 业务路由容器：公开组 + 受保护组。
///
/// ```ignore
/// let routes = AppRoutes::new()
///     .public(|r| r.route("/ping", get(ping)))          // 无需登录
///     .protected(|r| r.route("/me", get(me)));          // 无令牌 → 401
/// let app = build_router(routes, state);
/// ```
#[derive(Default)]
pub struct AppRoutes {
    public: Router<AppState>,
    protected: Router<AppState>,
}

impl AppRoutes {
    /// 空容器（两组都没有路由）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加**公开**路由（无需登录即可访问）。
    ///
    /// 每一项都应该能回答"为什么它可以匿名访问"；回答不了就说明放错了组。
    #[must_use]
    pub fn public(mut self, routes: impl FnOnce(Router<AppState>) -> Router<AppState>) -> Self {
        self.public = routes(self.public);
        self
    }

    /// 追加**受保护**路由（默认组）。
    #[must_use]
    pub fn protected(mut self, routes: impl FnOnce(Router<AppState>) -> Router<AppState>) -> Self {
        self.protected = routes(self.protected);
        self
    }

    /// 把一整棵路由树**全部**标为公开。
    ///
    /// 用于「本服务整体不需要鉴权」（如纯内网工具）或只测中间件行为的场景。
    /// 生产代码里出现它应当被 reviewer 追问。
    #[must_use]
    pub fn all_public(router: Router<AppState>) -> Self {
        Self {
            public: router,
            protected: Router::new(),
        }
    }

    /// 把一整棵路由树**全部**标为受保护（`Router` 的默认转换行为）。
    #[must_use]
    pub fn all_protected(router: Router<AppState>) -> Self {
        Self {
            public: Router::new(),
            protected: router,
        }
    }

    /// 公开组是否有路由。
    #[must_use]
    pub fn has_public(&self) -> bool {
        self.public.has_routes()
    }

    /// 受保护组是否有路由。
    #[must_use]
    pub fn has_protected(&self) -> bool {
        self.protected.has_routes()
    }
}

impl std::fmt::Debug for AppRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppRoutes")
            .field("has_public", &self.has_public())
            .field("has_protected", &self.has_protected())
            .finish()
    }
}

/// 裸 `Router` 视作**全部受保护**（fail-closed）。
///
/// 保留这个转换是为了让"只关心路由、不关心鉴权"的旧代码继续编译；
/// 语义上它不会把任何接口意外暴露出去。要公开请显式用
/// [`AppRoutes::public`] 或 [`AppRoutes::all_public`]。
impl From<Router<AppState>> for AppRoutes {
    fn from(router: Router<AppState>) -> Self {
        Self::all_protected(router)
    }
}

/// 组装路由。
///
/// - 合并 `telemetry::health` 的 `/health`、`/ready`（**始终公开**：探针不能要求令牌）
/// - 指标开启时挂载 `[telemetry].metrics.path`（默认 `/metrics`，自带抓取令牌校验）
/// - 受保护组套上「必须有 `Identity`」的守卫（feature `auth`）
/// - `fallback` → 404（`30001`）；`method_not_allowed_fallback` → 405（`10007`）
/// - 套上中间件栈（见 `stack`）
///
/// **装配期 panic**：若 `metrics.path` 与业务路由的 **GET** 冲突，axum 会 panic
/// （`Overlapping method route`）。axum 0.8 不暴露路由表，无法在这里做静态冲突
/// 检测；`metrics.path` 的形状已在 `TelemetrySection::validate_for_env` 里挡住
/// （含 `?` / `*` / `{` 等会直接 panic 的写法）。安全侧不受影响：鉴权旁路只对
/// GET/HEAD 生效（见 `stack`），POST 冲突不会导致跳过 JWT。
pub fn build_router(routes: impl Into<AppRoutes>, state: AppState) -> Router {
    let routes = routes.into();

    // 守卫层只作用于受保护组：`Router::layer` 在 merge 前应用，
    // 因此它位于全局中间件栈（含 `authenticate`）**内侧**，能看到已注入的 `Identity`。
    #[cfg(feature = "auth")]
    let protected = {
        warn_if_nothing_protected(&routes);
        routes.protected.layer(axum::middleware::from_fn(
            crate::http::middleware::auth_guard::require_identity,
        ))
    };
    #[cfg(not(feature = "auth"))]
    let protected = routes.protected;

    let router = Router::new()
        .merge(crate::telemetry::health::routes())
        .merge(routes.public)
        .merge(protected)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);

    // 指标端点（feature `metrics` + `[telemetry].metrics.enabled`）
    #[cfg(feature = "metrics")]
    let router = {
        let config = state.config.load();
        if config.telemetry.metrics.enabled {
            let path = config.telemetry.metrics.path.clone();
            // 把**装配期**的路径带进 handler：运行期改路径后旧路径立即 404，
            // 而不是「配置说搬走了，旧地址还在吐指标」。
            let registered = path.clone();
            router.route(
                &path,
                axum::routing::get(
                    move |state: State<AppState>, headers: HeaderMap| async move {
                        crate::telemetry::metrics::endpoint(state, headers, registered).await
                    },
                ),
            )
        } else {
            router
        }
    };

    let router = stack::apply(router, &state);
    router.with_state(state)
}

/// 启动期护栏：`auth` 已启用却一条受保护路由都没有时喊一声。
///
/// 这是「全都写进 public 组」的兜底提示——不阻断启动（有些服务确实全公开），
/// 但必须留痕，否则"忘了分组"和"刻意公开"在日志里长得一模一样。
#[cfg(feature = "auth")]
fn warn_if_nothing_protected(routes: &AppRoutes) {
    if !routes.has_protected() && routes.has_public() {
        tracing::warn!(
            "auth feature 已启用，但没有任何受保护路由（AppRoutes::protected 为空）：\
             全部业务接口都对匿名开放。若这是预期行为，请用 AppRoutes::all_public 明确表达"
        );
    }
}

/// 未匹配路由的兜底 handler（404 / `30001`）。
async fn not_found() -> AppError {
    AppError::not_found("route not found")
}

/// 方法不允许的兜底 handler（405 / `10007`）。
async fn method_not_allowed() -> AppError {
    AppError::biz(ErrorCode::METHOD_NOT_ALLOWED, "method not allowed")
}
