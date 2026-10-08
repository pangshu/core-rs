//! 认证（你是谁，文档 三·13）：session / jwt / oauth2，由配置选择，可并存。
//!
//! 认证只回答「你是谁」——统一产出 [`Identity`]（用户 id、角色、claims），
//! 由 `middleware/auth` 注入 extension，业务侧只认 `CurrentUser` 提取器，
//! 不关心底层是哪种认证方式。授权（能做什么）见 `authz/`（Casbin RBAC）。
//!
//! - [`jwt`]（feature = "jwt"）：无状态令牌，`Authorization: Bearer`；
//! - [`session`]（feature = "session"）：服务端会话，store 复用 cache 后端，
//!   凭 Cookie 的 session id 识别，适合需要**即时吊销**的场景；
//! - [`oauth2`]（feature = "oauth2"）：授权码 + PKCE 第三方登录客户端；
//!   换取用户信息后由应用落成 session / jwt，仍是统一 `Identity`。
//!
//! 凭据无效 → `Err`（401）；无凭据 → `Ok(None)`（交给下一方式）。
//! 链式组合（[`ChainAuthn`]）中单个方式的凭据错误**不阻断**其余方式：
//! 如 Bearer 已过期但 session cookie 有效时仍能以 session 登录，
//! 全部失败才返回第一个错误（保留 TokenExpired → 6401 语义）。

pub mod password; // 密码哈希子模块（argon2），三种认证方式共用

#[cfg(feature = "session")] // 仅在开启 session feature 时编译下一行
pub mod session; // 服务端会话认证子模块
#[cfg(feature = "jwt")] // 仅在开启 jwt feature 时编译下一行
pub mod jwt; // JWT 令牌认证子模块
#[cfg(feature = "oauth2")] // 仅在开启 oauth2 feature 时编译下一行
pub mod oauth2; // OAuth2 第三方登录客户端子模块

mod authn; // 认证契约与内置实现（Authn / ChainAuthn / Anonymous）
mod build; // 按 [auth] 配置装配认证链
mod identity; // 跨认证方式统一的身份类型

pub use authn::{Anonymous, Authn, ChainAuthn}; // 对外导出认证契约与内置实现
pub use build::build; // 对外导出认证链装配入口
pub use identity::Identity; // 对外导出统一身份类型
