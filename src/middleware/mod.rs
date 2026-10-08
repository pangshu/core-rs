//! 中间件库（01 文档的实现整体上移）：一个关注点一个文件。
//!
//! ## 两种装配模式（`App` 层二选一）
//!
//! **默认模式**（不调用 `App::bare`）：`App::serve` 自动装配完整推荐栈，
//! 应用零中间件代码。请求流（由外到内）：
//!
//! ```text
//! Extension(CoreState) → timeout → body_limit → security_headers → panic
//!       → request_id → locale → trace → access_log → compression → cors
//!       → [metrics::track] → ip_filter → rate_limit → csrf → auth
//!       → idempotency → handler
//! ```
//!
//! **裸骨架模式**（`App::bare()`）：`serve` 只挂三件**框架必需件**——
//! `Extension(CoreState)`（最外，供全部状态件与 `required()` 读取）、
//! panic 捕获、request_id（locale/trace/access_log 的依赖）；其余由应用在
//! 自己的路由树上自选，逐树不同：
//!
//! ```rust,ignore
//! let admin = stack::common(                      // 公共无状态层打包收尾
//!     admin::routes()                             // 由内向外挂状态件
//!         .layer(auth::require_identity_layer())  // 登录态兜底（无 Identity 401）
//!         .layer(idempotency::layer())
//!         .layer(auth::layer())
//!         .layer(ip_filter::layer()),
//!     &s.server,
//! );
//! App::<AppState>::bootstrap().await?
//!     .bare()
//!     .mount(admin)
//!     .mount(stack::common(open_api.layer(rate_limit::layer()), &s.server))
//!     .serve().await?;
//! ```
//!
//! 预设栈 [`stack::common`]（路由进路由出）打包 timeout/body_limit/
//! security_headers/cors/compression/locale/trace/access_log（config 关闭的
//! 件自动 no-op），应用无需背整条推荐顺序；各件也可用 [`BoxedLayer`] 构造器
//! 单独挂。配置 `enabled` 开关只决定"挂了是否生效"，挂载权（本模块的 layer
//! 构造器）归代码、生效权归配置，二者解耦且后者支持热更新。
//!
//! authz 不在链上：授权走路由级 `required()`（声明即校验，见 middleware/authz.rs）。
//!
//! 路由保护由应用决定：`App::mount` 只做合并、不套鉴权层；需要登录态的子树
//! 请自行 `.layer(auth::require_identity_layer())`（框架不做「公开/受保护」分类）。
//!
//! 组合权留应用：挂什么、挂哪层由应用的路由树决定；若偏离推荐顺序，以
//! 本模块注释为据（硬约束仅两条：request_id 须在 locale/trace/access_log
//! 外侧；auth 须在 idempotency 与 require_identity 外侧）。

pub mod access_log; // 访问日志中间件
pub mod auth; // 认证中间件（session/jwt/oauth2）
pub mod cors; // CORS 中间件（转发到 web/router 的实现）
pub mod idempotency; // 幂等中间件
pub mod ip_filter; // IP 过滤中间件
pub mod locale; // Locale 协商中间件
pub mod panic; // panic 统一响应体（配合 CatchPanicLayer）
#[cfg(feature = "rate-limit")] // 仅在开启 rate-limit feature 时编译下面模块
pub mod rate_limit; // 固定窗口限流中间件
pub mod request_id; // 双 id（request_id / trace_id）中间件
pub mod security_headers; // 安全响应头中间件
pub mod stack; // 预设栈帮手（裸模式自组装用）
pub mod timeout; // 请求超时层包装

#[cfg(feature = "csrf")] // 仅在开启 csrf feature 时编译下面模块
pub mod csrf; // CSRF 防护中间件
#[cfg(feature = "casbin")] // 仅在开启 casbin feature 时编译下面模块
pub mod authz; // 授权（Casbin 强制器）中间件

mod boxed_layer; // 自组装层统一返回类型（BoxedLayer）

pub use boxed_layer::BoxedLayer; // 对外导出装箱层类型，供各件 layer() 构造器使用

#[cfg(feature = "casbin")] // 仅在开启 casbin feature 时重导出
pub use authz::{required, required_in, RequiredPermission}; // 重导出路由级授权声明帮手
