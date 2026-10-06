//! 服务端会话（feature = "session"，文档 三·13）：store **复用 cache 后端**
//! （单机 memory / 多实例 redis），凭 Cookie 的 session id 识别；
//! 适合管理端、需要**即时吊销**的场景（destroy 立即生效）。

use std::sync::Arc;
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::HeaderMap;

use crate::cache::{CacheExt, CacheHandle};
use crate::config::sections::SessionSettings;
use crate::error::AppResult;

use super::{Authn, Identity};

/// 会话管理器：创建 / 读取（滑动续期）/ 吊销。登录登出由应用在 handler 里调用。
pub struct SessionManager {
    cache: CacheHandle,
    settings: SessionSettings,
    key_prefix: String,
}

impl SessionManager {
    pub fn new(cache: CacheHandle, settings: &SessionSettings) -> Self {
        Self {
            cache,
            settings: settings.clone(),
            key_prefix: "core-rs:sess:".to_string(),
        }
    }

    fn key(&self, sid: &str) -> String {
        format!("{}{}", self.key_prefix, sid)
    }

    /// 创建会话，返回 sid（应用负责 Set-Cookie，见 [`SessionManager::set_cookie_value`]）
    pub async fn create(&self, identity: &Identity) -> AppResult<String> {
        let sid = uuid::Uuid::new_v4().to_string();
        self.store(&sid, identity).await?;
        Ok(sid)
    }

    /// 读取会话并滑动续期
    pub async fn get(&self, sid: &str) -> AppResult<Option<Identity>> {
        let key = self.key(sid);
        match self.cache.get_json::<Identity>(&key).await? {
            Some(identity) => {
                // 滑动过期：每次访问续满 TTL
                let _ = self
                    .cache
                    .set_json(&key, &identity, Some(self.ttl()))
                    .await;
                Ok(Some(identity))
            }
            None => Ok(None),
        }
    }

    /// 即时吊销（登出 / 踢人）
    pub async fn destroy(&self, sid: &str) -> AppResult<()> {
        self.cache.del(&self.key(sid)).await?;
        Ok(())
    }

    /// 登录后的 Set-Cookie 值（HttpOnly；Secure 按 [auth.session] 配置）
    pub fn set_cookie_value(&self, sid: &str) -> String {
        let mut cookie = format!(
            "{}={sid}; Path=/; HttpOnly; SameSite=Lax",
            self.settings.cookie_name
        );
        if self.settings.secure {
            cookie.push_str("; Secure");
        }
        if !self.settings.domain.is_empty() {
            cookie.push_str("; Domain=");
            cookie.push_str(&self.settings.domain);
        }
        // Max-Age 与服务端 TTL 同源（滑动续期由服务端负责）：
        // 用 ttl() 的兜底值，避免 ttl_secs=0 时 Cookie 即刻过期而服务端保留会话
        cookie.push_str(&format!("; Max-Age={}", self.ttl().as_secs()));
        cookie
    }

    /// 登出后的清除 Cookie 值
    pub fn clear_cookie_value(&self) -> String {
        format!(
            "{}=; Path=/; HttpOnly; Max-Age=0",
            self.settings.cookie_name
        )
    }

    async fn store(&self, sid: &str, identity: &Identity) -> AppResult<()> {
        self.cache
            .set_json(&self.key(sid), identity, Some(self.ttl()))
            .await?;
        Ok(())
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.settings.ttl_secs.max(60))
    }
}

/// Cookie → Identity 的 [`Authn`] 实现
pub struct SessionAuthn {
    manager: Arc<SessionManager>,
    cookie_name: String,
}

impl SessionAuthn {
    pub fn new(manager: Arc<SessionManager>, settings: &SessionSettings) -> Self {
        Self {
            manager,
            cookie_name: settings.cookie_name.clone(),
        }
    }

    pub fn manager(&self) -> &SessionManager {
        &self.manager
    }
}

/// 从 Cookie 头解析 sid
fn sid_from_cookies(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    for cookie_header in headers.get_all(axum::http::header::COOKIE) {
        let raw = cookie_header.to_str().ok()?;
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == cookie_name {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

#[async_trait::async_trait]
impl Authn for SessionAuthn {
    fn name(&self) -> &str {
        "session"
    }

    async fn authenticate(&self, parts: &Parts) -> AppResult<Option<Identity>> {
        // 无凭据：交给链上的下一方式；有 sid 但会话失效 → 视为无凭据（匿名）
        let Some(sid) = sid_from_cookies(&parts.headers, &self.cookie_name) else {
            return Ok(None);
        };
        Ok(self.manager.get(&sid).await?)
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
