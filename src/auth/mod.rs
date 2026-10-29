//! 认证与授权（feature `auth` / `authz`）。
//!
//! - [`AuthService`]：装配入口（`AppState.auth`），持有 JWT 编解码器；
//!   启用 `authz` 时再挂 RBAC 授权器
//! - [`Identity`] / [`Principal`] / [`OptionalPrincipal`]：请求身份提取器
//! - [`Claims`] / [`TokenPair`]：JWT 载荷与令牌对
//! - [`authenticate`]：解析 `Authorization` 并注入 `Identity` 的中间件
//! - `casbin`（feature `authz`）：RBAC 策略引擎
//! - `session`（feature `session`）：服务端会话 + CSRF
//! - `revocation`（feature `revocation`）：jti 黑名单 + 刷新令牌轮换 / 重用检测
//!
//! 文件划分：`jwt` / `casbin` / `identity` / `middleware` / `session` / `service`。
//!
//! **`auth` 与 `authz` 分开**：`auth` 只引入 `jsonwebtoken`；`authz` 才引入
//! `casbin`（它会拖进 `rhai` 脚本引擎 + `petgraph` + `regex`）。只用 JWT 的应用
//! 不应该为 RBAC 付编译代价。

#[cfg(feature = "authz")]
pub mod casbin;
pub mod identity;
pub mod jwt;
pub mod middleware;
#[cfg(feature = "revocation")]
pub mod revocation;
#[cfg(feature = "revocation")]
pub mod rotation;
mod service;
#[cfg(feature = "session")]
pub mod session;

#[cfg(feature = "authz")]
pub use casbin::Authorizer;
pub use identity::{AuthMethod, Identity, OptionalPrincipal, Principal, VerifiedClaims};
pub use jwt::{ACCESS_TOKEN, BEARER_PREFIX, Claims, JwtCodec, REFRESH_TOKEN, TokenPair};
pub use middleware::authenticate;
#[cfg(feature = "revocation")]
pub use revocation::RevocationStore;
#[cfg(feature = "revocation")]
pub use rotation::RefreshRotation;
pub use service::AuthService;
#[cfg(feature = "session-redis")]
pub use session::RedisSessionStore;
#[cfg(feature = "session")]
pub use session::{MemorySessionStore, Session, SessionStore, session_middleware};
