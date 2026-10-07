//! 服务端会话（feature = "session"，文档 三·13）：store **复用 cache 后端**
//! （单机 memory / 多实例 redis），凭 Cookie 的 session id 识别；
//! 适合管理端、需要**即时吊销**的场景（destroy 立即生效）。

use std::sync::Arc; // 引入原子引用计数指针，跨请求共享会话管理器
use std::time::Duration; // 引入时长类型，表示会话 TTL

use axum::http::request::Parts; // 引入请求部件类型，认证时读取 Cookie 头
use axum::http::HeaderMap; // 引入请求头集合类型

use crate::cache::{CacheExt, CacheHandle}; // 引入缓存扩展 trait 与缓存句柄
use crate::config::sections::SessionSettings; // 引入会话相关配置结构
use crate::error::AppResult; // 引入框架统一结果别名

use super::{Authn, Identity}; // 引入认证契约与统一身份类型

/// 会话管理器：创建 / 读取（滑动续期）/ 吊销。登录登出由应用在 handler 里调用。
pub struct SessionManager { // 定义会话管理器
    cache: CacheHandle, // 会话存储后端（复用可插拔缓存）
    settings: SessionSettings, // 会话相关配置（Cookie 名、TTL、Secure 等）
    key_prefix: String, // 缓存键前缀，隔离会话数据
}

impl SessionManager { // 为会话管理器提供方法
    pub fn new(cache: CacheHandle, settings: &SessionSettings) -> Self { // 由缓存句柄与配置构造
        Self { // 组装管理器
            cache, // 存入缓存句柄
            settings: settings.clone(), // 克隆并保存会话配置
            key_prefix: "core-rs:sess:".to_string(), // 固定键前缀，避免与其他缓存键冲突
        }
    }

    fn key(&self, sid: &str) -> String { // 由 sid 拼出缓存键
        format!("{}{}", self.key_prefix, sid) // 前缀 + session id
    }

    /// 创建会话，返回 sid（应用负责 Set-Cookie，见 [`SessionManager::set_cookie_value`]）
    pub async fn create(&self, identity: &Identity) -> AppResult<String> { // 创建新会话
        let sid = uuid::Uuid::new_v4().to_string(); // 生成随机不可猜测的 session id
        self.store(&sid, identity).await?; // 把身份写入缓存存储
        Ok(sid) // 返回 sid 供应用下发 Cookie
    }

    /// 读取会话并滑动续期
    pub async fn get(&self, sid: &str) -> AppResult<Option<Identity>> { // 读取会话身份
        let key = self.key(sid); // 计算缓存键
        match self.cache.get_json::<Identity>(&key).await? { // 从缓存取会话身份
            Some(identity) => { // 命中会话
                // 滑动过期：每次访问续满 TTL
                let _ = self // 忽略续期写回的错误（失败不影响本次读取）
                    .cache // 使用缓存后端
                    .set_json(&key, &identity, Some(self.ttl())) // 以满 TTL 重写会话，实现滑动续期
                    .await;
                Ok(Some(identity)) // 返回身份
            }
            None => Ok(None), // 未命中视为会话失效（匿名）
        }
    }

    /// 即时吊销（登出 / 踢人）
    pub async fn destroy(&self, sid: &str) -> AppResult<()> { // 删除会话，立即生效
        self.cache.del(&self.key(sid)).await?; // 从缓存删除该 sid
        Ok(()) // 返回成功
    }

    /// 登录后的 Set-Cookie 值（HttpOnly；Secure 按 [auth.session] 配置）
    pub fn set_cookie_value(&self, sid: &str) -> String { // 生成登录 Cookie 字符串
        let mut cookie = format!( // 以配置的 Cookie 名组装基础属性
            "{}={sid}; Path=/; HttpOnly; SameSite=Lax", // HttpOnly 防脚本读取，Lax 缓解 CSRF
            self.settings.cookie_name
        );
        if self.settings.secure { // 配置要求 Secure 时
            cookie.push_str("; Secure"); // 追加 Secure，仅 HTTPS 传输
        }
        if !self.settings.domain.is_empty() { // 配置了域名时
            cookie.push_str("; Domain="); // 追加 Domain 前缀
            cookie.push_str(&self.settings.domain); // 写入域名
        }
        // Max-Age 与服务端 TTL 同源（滑动续期由服务端负责）：
        // 用 ttl() 的兜底值，避免 ttl_secs=0 时 Cookie 即刻过期而服务端保留会话
        cookie.push_str(&format!("; Max-Age={}", self.ttl().as_secs())); // 追加与服务端一致的过期秒数
        cookie // 返回完整 Cookie 字符串
    }

    /// 登出后的清除 Cookie 值
    pub fn clear_cookie_value(&self) -> String { // 生成清除 Cookie 字符串
        format!(
            "{}=; Path=/; HttpOnly; Max-Age=0", // 置空值并立即过期
            self.settings.cookie_name
        )
    }

    async fn store(&self, sid: &str, identity: &Identity) -> AppResult<()> { // 把身份写入会话存储
        self.cache // 使用缓存后端
            .set_json(&self.key(sid), identity, Some(self.ttl())) // 以 TTL 写入 JSON
            .await?;
        Ok(()) // 返回成功
    }

    fn ttl(&self) -> Duration { // 计算会话 TTL
        Duration::from_secs(self.settings.ttl_secs.max(60)) // 至少 60 秒，避免配置 0 导致立即过期
    }
}

/// Cookie → Identity 的 [`Authn`] 实现
pub struct SessionAuthn { // 把会话管理器接入统一认证链的适配器
    manager: Arc<SessionManager>, // 共享的会话管理器
    cookie_name: String, // 用于定位会话的 Cookie 名
}

impl SessionAuthn { // 为 SessionAuthn 提供构造与访问
    pub fn new(manager: Arc<SessionManager>, settings: &SessionSettings) -> Self { // 由管理器与配置构造
        Self {
            manager, // 存入共享管理器
            cookie_name: settings.cookie_name.clone(), // 记录 Cookie 名
        }
    }

    pub fn manager(&self) -> &SessionManager { // 暴露内部管理器，供应用创建/吊销会话
        &self.manager // 返回内部引用
    }
}

/// 从 Cookie 头解析 sid
fn sid_from_cookies(headers: &HeaderMap, cookie_name: &str) -> Option<String> { // 从请求头中提取指定 Cookie 值
    for cookie_header in headers.get_all(axum::http::header::COOKIE) { // 遍历所有 Cookie 头（可能多个）
        let raw = cookie_header.to_str().ok()?; // 转成合法字符串，非法则整体放弃
        for pair in raw.split(';') { // 按分号拆分键值对
            let pair = pair.trim(); // 去掉两侧空白
            if let Some((k, v)) = pair.split_once('=') { // 拆出键与值
                if k.trim() == cookie_name { // 键名匹配目标 Cookie
                    return Some(v.trim().to_string()); // 返回值部分作为 sid
                }
            }
        }
    }
    None // 未找到则返回 None
}

#[async_trait::async_trait] // 为下面的实现启用 async 支持
impl Authn for SessionAuthn { // 让 SessionAuthn 实现认证契约
    fn name(&self) -> &str { // 返回认证方式名
        "session" // 固定标识为 session
    }

    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>> { // 从 Cookie 识别身份
        // 无凭据：交给链上的下一方式；有 sid 但会话失效 → 视为无凭据（匿名）
        let Some(sid) = sid_from_cookies(&parts.headers, &self.cookie_name) else { // 尝试解析 sid
            return Ok(None); // 无 sid 视为匿名
        };
        Ok(self.manager.get(&sid).await?) // 用 sid 查会话并滑动续期
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("a=1; core_rs_session=abc-123; b=2"),
        );
        assert_eq!(sid_from_cookies(&headers, "core_rs_session").as_deref(), Some("abc-123"));
        assert_eq!(sid_from_cookies(&headers, "other"), None);
    }
}
