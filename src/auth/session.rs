//! 会话认证（feature `session`）。
//!
//! - Cookie 属性：`HttpOnly` + `Secure`（生产）+ `SameSite`（见 `docs/11` §2.3）
//! - 存储：**单实例内存** / 多实例 Redis（`session-redis`）
//! - 登录成功后**重新生成 session id**（防会话固定攻击）

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{FromRequestParts, Request};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderValue, request::Parts};
use axum::middleware::Next;
use axum::response::Response;
use tokio::sync::Mutex;

use crate::app::state::AppState;
use crate::auth::identity::Principal;
use crate::config::sections::auth::SessionSection;
use crate::errors::{AppError, AppResult};
use crate::telemetry::new_trace_id;

/// 会话存储。
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// 读取会话数据（序列化的 [`Principal`]）。
    async fn get(&self, id: &str) -> AppResult<Option<Vec<u8>>>;

    /// 写入会话数据并设置 TTL。
    async fn set(&self, id: &str, data: &[u8], ttl: Duration) -> AppResult<()>;

    /// 删除会话。
    async fn remove(&self, id: &str) -> AppResult<()>;
}

/// 内存会话条目：数据 + 到期时刻。
#[derive(Debug)]
struct SessionEntry {
    data: Vec<u8>,
    expires_at: Instant,
}

/// 条目数超过该阈值时，在写入路径顺带清理过期项。
///
/// 只写不读的会话不会触发 `get` 的惰性清理，若不主动 prune 会长期占用内存。
const PRUNE_THRESHOLD: usize = 1024;

/// 内存会话存储（单实例；多实例请用 `session-redis`）。
///
/// 与 `session-redis` 语义对齐：**每次 `set` 都按新 TTL 重新计时**，`get` 命中过期项
/// 视为不存在并顺手删除。
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    inner: Mutex<std::collections::HashMap<String, SessionEntry>>,
}

impl MemorySessionStore {
    /// 构造内存存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn get(&self, id: &str) -> AppResult<Option<Vec<u8>>> {
        let mut map = self.inner.lock().await;
        let now = Instant::now();
        if map.get(id).is_some_and(|entry| entry.expires_at <= now) {
            map.remove(id);
            return Ok(None);
        }
        Ok(map.get(id).map(|entry| entry.data.clone()))
    }

    async fn set(&self, id: &str, data: &[u8], ttl: Duration) -> AppResult<()> {
        if ttl.is_zero() {
            return Err(AppError::invalid_param("会话 TTL 必须大于 0"));
        }
        let mut map = self.inner.lock().await;
        if map.len() >= PRUNE_THRESHOLD {
            let now = Instant::now();
            map.retain(|_, entry| entry.expires_at > now);
        }
        map.insert(
            id.to_string(),
            SessionEntry {
                data: data.to_vec(),
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn remove(&self, id: &str) -> AppResult<()> {
        self.inner.lock().await.remove(id);
        Ok(())
    }
}

/// Redis 会话存储（`session-redis`）：多实例部署共享会话。
///
/// 键格式 `{namespace}:s:{id}`（`namespace` 取 `[cache].namespace`）；
/// 连接复用 `[cache].redis.url`，与缓存后端共享同一套 Redis 配置。
#[cfg(feature = "session-redis")]
#[derive(Clone)]
pub struct RedisSessionStore {
    conn: redis::aio::ConnectionManager,
    prefix: String,
}

#[cfg(feature = "session-redis")]
impl RedisSessionStore {
    /// 按连接串建立连接。
    ///
    /// # Errors
    /// URL 非法或连接失败时返回 `50004`（`CACHE_ERROR`）。
    pub async fn connect(url: &str, namespace: &str) -> AppResult<Self> {
        // 建连超时 + 命令响应超时都在 `utils::redis::connect` 里设好
        let conn = crate::utils::redis::connect(url)
            .await
            .map_err(map_redis_err)?;
        Ok(Self {
            conn,
            prefix: format!("{namespace}:s"),
        })
    }

    fn key(&self, id: &str) -> String {
        format!("{}:{}", self.prefix, id)
    }
}

#[cfg(feature = "session-redis")]
impl std::fmt::Debug for RedisSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisSessionStore")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "session-redis")]
#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn get(&self, id: &str) -> AppResult<Option<Vec<u8>>> {
        let mut conn = self.conn.clone();
        let value: Option<Vec<u8>> = redis::cmd("GET")
            .arg(self.key(id))
            .query_async(&mut conn)
            .await
            .map_err(map_redis_err)?;
        Ok(value)
    }

    async fn set(&self, id: &str, data: &[u8], ttl: Duration) -> AppResult<()> {
        let mut conn = self.conn.clone();
        let millis = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1);
        let _: String = redis::cmd("SET")
            .arg(self.key(id))
            .arg(data)
            .arg("PX")
            .arg(millis)
            .query_async(&mut conn)
            .await
            .map_err(map_redis_err)?;
        Ok(())
    }

    async fn remove(&self, id: &str) -> AppResult<()> {
        let mut conn = self.conn.clone();
        let _: i64 = redis::cmd("DEL")
            .arg(self.key(id))
            .query_async(&mut conn)
            .await
            .map_err(map_redis_err)?;
        Ok(())
    }
}

#[cfg(feature = "session-redis")]
use crate::errors::ErrorCode;

#[cfg(feature = "session-redis")]
fn map_redis_err(error: redis::RedisError) -> AppError {
    tracing::error!(%error, "redis 会话存储访问失败");
    AppError::biz(ErrorCode::CACHE_ERROR, "会话存储访问失败")
}

/// 会话 Cookie 的动作。
#[derive(Debug, Clone)]
enum CookieAction {
    /// 不改动 Cookie。
    None,
    /// 下发（或刷新）会话 Cookie。
    Set(String),
    /// 清除会话 Cookie。
    Clear,
}

/// 会话内部状态。
#[derive(Debug)]
struct SessionState {
    id: String,
    principal: Option<Principal>,
    action: CookieAction,
}

/// 会话内部共享体。
struct SessionInner {
    store: Arc<dyn SessionStore>,
    config: SessionSection,
    state: Mutex<SessionState>,
}

impl std::fmt::Debug for SessionInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionInner")
            .field("cookie_name", &self.config.cookie_name)
            .field("ttl", &self.config.ttl)
            .finish_non_exhaustive()
    }
}

/// 会话（`Arc` 共享，可在 handler 间传递）。
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

impl Session {
    /// 当前会话 id。
    pub async fn id(&self) -> String {
        self.inner.state.lock().await.id.clone()
    }

    /// 当前已登录主体（未登录为 `None`）。
    ///
    /// # Errors
    /// 存储读取或反序列化失败时返回错误。
    pub async fn principal(&self) -> AppResult<Option<Principal>> {
        let (id, cached) = {
            let state = self.inner.state.lock().await;
            (state.id.clone(), state.principal.clone())
        };
        if let Some(principal) = cached {
            return Ok(Some(principal));
        }
        let Some(data) = self.inner.store.get(&id).await? else {
            return Ok(None);
        };
        let principal: Principal = serde_json::from_slice(&data)
            .map_err(|e| AppError::internal(anyhow::anyhow!("invalid session data: {e}")))?;
        self.inner.state.lock().await.principal = Some(principal.clone());
        Ok(Some(principal))
    }

    /// 登录：写入会话并**重新生成 session id**（防会话固定攻击）。
    ///
    /// # Errors
    /// 存储写入失败时返回错误。
    pub async fn login(&self, principal: &Principal) -> AppResult<()> {
        let new_id = new_trace_id();
        let data = serde_json::to_vec(principal)
            .map_err(|e| AppError::internal(anyhow::anyhow!("serialize session: {e}")))?;
        self.inner
            .store
            .set(&new_id, &data, self.inner.config.ttl)
            .await?;

        let mut state = self.inner.state.lock().await;
        state.id = new_id.clone();
        state.principal = Some(principal.clone());
        state.action = CookieAction::Set(new_id);
        Ok(())
    }

    /// 登出：删除会话并清除 Cookie。
    ///
    /// # Errors
    /// 存储删除失败时返回错误。
    pub async fn logout(&self) -> AppResult<()> {
        let id = self.inner.state.lock().await.id.clone();
        self.inner.store.remove(&id).await?;
        let mut state = self.inner.state.lock().await;
        state.principal = None;
        state.action = CookieAction::Clear;
        Ok(())
    }

    /// 要求已登录，未登录返回 `20001`（401）。
    ///
    /// # Errors
    /// 未登录或存储失败时返回错误。
    pub async fn require_principal(&self) -> AppResult<Principal> {
        self.principal()
            .await?
            .ok_or_else(|| AppError::unauthenticated("authentication required"))
    }
}

impl FromRequestParts<AppState> for Session {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Session>()
            .cloned()
            .ok_or_else(|| AppError::internal(anyhow::anyhow!("session middleware not installed")))
    }
}

/// 构造会话中间件。
///
/// 读取会话 Cookie → 加载主体 → 注入 [`Session`] 扩展；handler 内
/// `login` / `logout` 会在响应上设置或清除 Cookie。
pub fn session_middleware(
    config: SessionSection,
    store: Arc<dyn SessionStore>,
    req: Request,
    next: Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move {
        let existing_id = read_cookie(&req, &config.cookie_name);

        let state = SessionState {
            id: existing_id.unwrap_or_else(new_trace_id),
            principal: None,
            action: CookieAction::None,
        };
        let inner = Arc::new(SessionInner {
            store,
            config: config.clone(),
            state: Mutex::new(state),
        });
        let session = Session {
            inner: Arc::clone(&inner),
        };

        let mut req = req;
        req.extensions_mut().insert(session);
        let mut response = next.run(req).await;

        let action = inner.state.lock().await.action.clone();
        match action {
            CookieAction::Set(id) => {
                if let Ok(value) = HeaderValue::from_str(&cookie_value(&config, &id, true)) {
                    response.headers_mut().append(SET_COOKIE, value);
                }
            }
            CookieAction::Clear => {
                if let Ok(value) = HeaderValue::from_str(&cookie_value(&config, "", false)) {
                    response.headers_mut().append(SET_COOKIE, value);
                }
            }
            CookieAction::None => {}
        }

        response
    })
}

/// 从 `Cookie` 头读取指定 Cookie 值。
fn read_cookie(req: &Request, name: &str) -> Option<String> {
    let raw = req.headers().get(COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let mut it = pair.trim().splitn(2, '=');
        if it.next()? == name {
            return it.next().map(str::to_string);
        }
    }
    None
}

/// 构造 `Set-Cookie` 值（`HttpOnly` + 可选 `Secure` + `SameSite`）。
fn cookie_value(config: &SessionSection, id: &str, keep: bool) -> String {
    use crate::config::sections::auth::SameSite;

    let mut parts = vec![format!("{}={}", config.cookie_name, id)];
    parts.push("Path=/".to_string());
    parts.push("HttpOnly".to_string());
    if config.secure {
        parts.push("Secure".to_string());
    }
    match config.same_site {
        SameSite::Strict => parts.push("SameSite=Strict".to_string()),
        SameSite::Lax => parts.push("SameSite=Lax".to_string()),
        SameSite::None => parts.push("SameSite=None".to_string()),
    }
    if keep {
        parts.push(format!("Max-Age={}", config.ttl.as_secs()));
    } else {
        parts.push("Max-Age=0".to_string());
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SessionSection {
        SessionSection {
            enabled: true,
            cookie_name: "sid".to_string(),
            ttl: Duration::from_secs(60),
            secure: true,
            same_site: crate::config::sections::auth::SameSite::Lax,
            store: crate::config::sections::auth::SessionStoreKind::Memory,
        }
    }

    async fn new_session() -> Session {
        let inner = Arc::new(SessionInner {
            store: Arc::new(MemorySessionStore::new()),
            config: config(),
            state: Mutex::new(SessionState {
                id: "old-id".to_string(),
                principal: None,
                action: CookieAction::None,
            }),
        });
        Session { inner }
    }

    #[tokio::test]
    async fn should_login_rotate_id_and_restore_principal() {
        let session = new_session().await;
        let principal = Principal::from_id(9).with_role("user");

        session.login(&principal).await.unwrap();
        assert_ne!(session.id().await, "old-id", "登录应重新生成 session id");
        assert_eq!(session.principal().await.unwrap(), Some(principal));
    }

    #[tokio::test]
    async fn should_logout_clear_principal() {
        let session = new_session().await;
        session.login(&Principal::new("u")).await.unwrap();
        session.logout().await.unwrap();

        assert!(session.principal().await.unwrap().is_none());
        assert!(matches!(
            session.inner.state.lock().await.action,
            CookieAction::Clear
        ));
    }

    #[tokio::test]
    async fn should_require_principal_return_401_when_anonymous() {
        let session = new_session().await;
        let err = session.require_principal().await.unwrap_err();
        assert_eq!(err.code(), crate::errors::ErrorCode::UNAUTHENTICATED);
    }

    #[test]
    fn should_build_cookie_value() {
        let value = cookie_value(&config(), "abc", true);
        assert!(value.contains("sid=abc"));
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("Secure"));
        assert!(value.contains("SameSite=Lax"));
        assert!(value.contains("Max-Age=60"));
    }

    #[test]
    fn should_clear_cookie_with_zero_max_age() {
        let value = cookie_value(&config(), "", false);
        assert!(value.contains("sid="));
        assert!(value.contains("Max-Age=0"));
    }

    // —— 内存会话存储的 TTL 语义（与 session-redis 对齐）——

    #[tokio::test]
    async fn should_expire_memory_session_after_ttl() {
        let store = MemorySessionStore::new();
        store
            .set("sid-1", b"data", Duration::from_millis(30))
            .await
            .unwrap();
        assert!(store.get("sid-1").await.unwrap().is_some());

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            store.get("sid-1").await.unwrap().is_none(),
            "超过 TTL 后应视为不存在"
        );
    }

    #[tokio::test]
    async fn should_refresh_memory_session_ttl_on_update() {
        let store = MemorySessionStore::new();
        store
            .set("sid-2", b"v1", Duration::from_millis(200))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // 覆盖写：按新 TTL 重新计时
        store
            .set("sid-2", b"v2", Duration::from_millis(200))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            store.get("sid-2").await.unwrap().as_deref(),
            Some(&b"v2"[..]),
            "覆盖写应刷新到期时间"
        );
    }

    #[tokio::test]
    async fn should_reject_zero_ttl_for_memory_session() {
        let store = MemorySessionStore::new();
        assert!(
            store.set("sid-3", b"v", Duration::ZERO).await.is_err(),
            "TTL 为 0 不得写入（否则等于永不过期）"
        );
    }
}
