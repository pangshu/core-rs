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

use axum::http::request::Parts; // 引入 axum 的请求部件类型，认证时只读请求头等

use crate::error::AppResult; // 引入框架统一结果别名

/// 认证身份（框架统一产物；session 存储需要 serde 序列化）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)] // 派生调试/克隆与 serde 序列化，供 session 缓存存取
pub struct Identity { // 定义跨认证方式统一的身份结构
    /// 用户标识（业务侧的 user id 字符串化）
    pub id: String, // 用户唯一标识
    /// 角色列表（authz 层与业务权限判断共用）
    pub roles: Vec<String>, // 角色名列表
    /// 完整 claims（JWT 自定义字段 / OAuth2 userinfo / session 快照）
    pub claims: serde_json::Value, // 原始 claims JSON，保留方式特有字段
    /// 认证来源："jwt" | "session" | "oauth2:<provider>"
    pub source: String, // 标记本次身份由哪种方式识别
}

impl Identity { // 为 Identity 提供构造与链式扩展
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Self { // 构造最小身份：仅 id 与来源
        Self { // 组装结构体
            id: id.into(), // 转换并存入用户标识
            roles: Vec::new(), // 角色先置空，可由 with_roles 补充
            claims: serde_json::Value::Null, // claims 先置 null，可由 with_claims 补充
            source: source.into(), // 转换并存入认证来源
        }
    }

    pub fn with_roles(mut self, roles: Vec<String>) -> Self { // 链式设置角色列表
        self.roles = roles; // 覆盖角色字段
        self // 返回自身以支持链式调用
    }

    pub fn with_claims(mut self, claims: serde_json::Value) -> Self { // 链式设置完整 claims
        self.claims = claims; // 覆盖 claims 字段
        self // 返回自身以支持链式调用
    }
}

/// 认证方式契约
#[async_trait::async_trait] // 让 trait 支持 async 方法（编译期改写为返回 Future）
pub trait Authn: Send + Sync { // 定义认证器契约，要求可跨线程共享
    fn name(&self) -> &str; // 返回该认证方式的名称（用于日志/调试）
    /// 从请求中识别身份。无凭据 → `None`；凭据无效 → `Err`（401）。
    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>>; // 核心方法：从请求解析身份
}

/// 组合认证：按配置顺序（`[auth].mode`）逐个尝试，任一命中即返回
pub struct ChainAuthn { // 定义认证链，把多个方式串起来
    schemes: Vec<std::sync::Arc<dyn Authn>>, // 按顺序保存各认证器的共享引用
}

impl ChainAuthn { // 为认证链提供构造
    pub fn new(schemes: Vec<std::sync::Arc<dyn Authn>>) -> Self { // 用有序认证器列表构造
        Self { schemes } // 直接存入列表
    }
}

#[async_trait::async_trait] // 为下面的 trait 实现启用 async 支持
impl Authn for ChainAuthn { // 让 ChainAuthn 实现 Authn 契约
    fn name(&self) -> &str { // 返回链式认证的名称
        "chain" // 固定标识为 chain
    }

    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>> { // 依次尝试每个认证器
        // 单个 scheme 凭据无效不阻断其余 scheme：Bearer 过期 + session cookie
        // 有效时应能用 session 登录，而不是被硬 401 登出
        let mut first_err: Option<crate::error::AppError> = None; // 记录首个错误，供全部失败时返回
        for scheme in &self.schemes { // 按配置顺序遍历认证器
            match scheme.authenticate(parts).await { // 调用当前认证器识别身份
                Ok(Some(identity)) => return Ok(Some(identity)), // 任一命中立即返回该身份
                Ok(None) => {} // 无凭据：继续尝试下一个方式
                Err(e) => { // 凭据无效：记录但不立即中断
                    if first_err.is_none() { // 仅保留第一个错误
                        first_err = Some(e); // 存下首个错误
                    }
                }
            }
        }
        match first_err { // 全部方式尝试完毕后决定返回
            Some(e) => Err(e), // 有错误则返回首个错误（保留 6401 语义）
            None => Ok(None), // 全部无凭据则视为匿名
        }
    }
}

/// 匿名认证（不启用任何方式时的占位）：恒返回 None，接口全匿名，
/// 需要登录态的接口由 `CurrentUser` 提取器返回 401
pub struct Anonymous; // 空结构体，仅作占位标记

#[async_trait::async_trait] // 为匿名认证实现启用 async 支持
impl Authn for Anonymous { // 让 Anonymous 实现 Authn 契约
    fn name(&self) -> &str { // 返回匿名认证的名称
        "anonymous" // 固定标识为 anonymous
    }
    async fn authenticate(&self, _parts: &Parts) -> AppResult<Option<Identity>> { // 匿名认证不解析请求
        Ok(None) // 恒返回无身份，交由后续提取器决定是否 401
    }
}

/// 按 `[auth]` 配置装配认证链（App 装配时自动调用；cache 供 session store 使用）
#[allow(unused_variables)] // 某些 feature 组合下参数可能未使用，抑制告警
pub fn build( // 定义认证链装配入口
    settings: &crate::config::sections::AuthSettings, // 认证相关配置（mode、jwt、session 等）
    cache: &crate::cache::CacheHandle, // 缓存句柄，供 session store 复用
) -> AppResult<Option<std::sync::Arc<dyn Authn>>> { // 返回可选的认证器（未启用任何方式时为 None）
    #[allow(unused_mut)] // 部分 feature 关闭时无 push
    let mut schemes: Vec<std::sync::Arc<dyn Authn>> = Vec::new(); // 收集启用的认证器
    for mode in settings.modes() { // 按配置中的认证方式顺序遍历
        match mode.as_str() { // 按方式名分派装配逻辑
            "jwt" => { // 处理 jwt 方式
                #[cfg(feature = "jwt")] // 仅在开启 jwt feature 时编译下一行
                if !settings.jwt.secret.is_empty() { // secret 已配置才真正启用
                    schemes.push(std::sync::Arc::new(jwt::JwtAuthn::new(&settings.jwt)?)); // 构造并加入 JWT 认证器
                    continue; // 已加入则跳过后续告警逻辑
                }
                let _ = &settings; // 未开启 feature 时消除未使用参数告警
                tracing::warn!("auth.mode includes jwt but [auth.jwt].secret is empty / feature disabled"); // 配置了 jwt 但不可用，告警
            }
            "session" => { // 处理 session 方式
                #[cfg(feature = "session")] // 仅在开启 session feature 时编译下面块
                { // session 装配块开始
                    schemes.push(std::sync::Arc::new(session::SessionAuthn::new( // 构造会话认证器并加入链
                        std::sync::Arc::new(session::SessionManager::new( // 先构造共享的会话管理器
                            cache.clone(), // 复用缓存句柄作为 session store
                            &settings.session, // 传入会话相关配置
                        )),
                        &settings.session, // 传入会话配置供 Cookie 名等使用
                    )));
                    continue; // 已加入则跳过后续告警逻辑
                }
                #[cfg(not(feature = "session"))] // 未开启 session feature 时编译下一行
                tracing::warn!("auth.mode includes session but feature `session` is disabled"); // feature 未开，告警
            }
            "oauth2" => { // 处理 oauth2 配置项
                // OAuth2 是「换取身份」的客户端流程，不作为请求认证方式参与链；
                // 应用在回调里用其结果落 session / jwt（见 oauth2 模块文档）
            }
            other => { // 未知方式名
                tracing::warn!(mode = %other, "unknown auth.mode, ignored"); // 记录并忽略，不影响其他方式
            }
        }
    }
    if schemes.is_empty() { // 没有任何可用方式
        Ok(None) // 返回 None，等价于全匿名
    } else if schemes.len() == 1 { // 仅一种方式
        Ok(schemes.into_iter().next()) // 直接返回该认证器，免去多余的链包装
    } else { // 多种方式
        Ok(Some(std::sync::Arc::new(ChainAuthn::new(schemes)))) // 包装成链式认证
    }
}
