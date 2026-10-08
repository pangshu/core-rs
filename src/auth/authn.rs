//! 认证方式契约与内置实现：[`Authn`] 契约、链式组合 [`ChainAuthn`]、占位 [`Anonymous`]。
//!
//! 凭据无效 → `Err`（401）；无凭据 → `Ok(None)`（交给下一方式）。
//! 链式组合中单个方式的凭据错误**不阻断**其余方式：如 Bearer 已过期但
//! session cookie 有效时仍能以 session 登录，全部失败才返回第一个错误
//! （保留 TokenExpired → 6401 语义）。

use axum::http::request::Parts; // 引入 axum 的请求部件类型，认证时只读请求头等

use crate::error::AppResult; // 引入框架统一结果别名

use super::Identity; // 引入统一身份类型

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
