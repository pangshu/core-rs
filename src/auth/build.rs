//! 按 `[auth]` 配置装配认证链（App 装配时自动调用；cache 供 session store 使用）。
//!
//! 链式组合的语义见 [`super::ChainAuthn`]：按 `[auth].mode` 顺序逐个尝试，
//! 任一命中即返回；全部无凭据视为匿名，全部失败返回首个错误。

use crate::error::AppResult; // 引入框架统一结果别名

use super::authn::{Authn, ChainAuthn}; // 引入认证契约与链式组合

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
                    schemes.push(std::sync::Arc::new(super::jwt::JwtAuthn::new(&settings.jwt)?)); // 构造并加入 JWT 认证器
                    continue; // 已加入则跳过后续告警逻辑
                }
                let _ = &settings; // 未开启 feature 时消除未使用参数告警
                tracing::warn!("auth.mode includes jwt but [auth.jwt].secret is empty / feature disabled"); // 配置了 jwt 但不可用，告警
            }
            "session" => { // 处理 session 方式
                #[cfg(feature = "session")] // 仅在开启 session feature 时编译下面块
                { // session 装配块开始
                    schemes.push(std::sync::Arc::new(super::session::SessionAuthn::new( // 构造会话认证器并加入链
                        std::sync::Arc::new(super::session::SessionManager::new( // 先构造共享的会话管理器
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
