//! 认证授权服务：JWT 编解码 +（可选）RBAC 授权 +（可选）会话存储。

#[cfg(feature = "authz")]
use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "authz")]
use crate::auth::casbin::Authorizer;
use crate::auth::identity::Principal;
use crate::auth::jwt::{Claims, JwtCodec, TokenPair};
#[cfg(feature = "session")]
use crate::auth::session::SessionStore;
use crate::config::sections::auth::AuthSection;
#[cfg(feature = "authz")]
use crate::config::sections::auth::{AuthzAdapter, AuthzSection};
use crate::errors::AppResult;

/// 认证授权服务（`AppState.auth`）。
///
/// 以 `Arc` 共享；handler 经 `State<AppState>`（或 `State<AuthService>`，需
/// `FromRef`）访问。
#[derive(Clone)]
pub struct AuthService {
    jwt: Arc<JwtCodec>,
    /// RBAC 授权器（feature `authz`）。
    ///
    /// `None` = 只装了 JWT、没装策略引擎（`from_config` 之后没调
    /// `with_authorizer_from_config`）。**`None` 时一律拒绝**——没有策略引擎
    /// 不等于「全部放行」，那是 fail-open。
    #[cfg(feature = "authz")]
    authorizer: Option<Arc<Authorizer>>,
    #[cfg(feature = "session")]
    sessions: Arc<dyn SessionStore>,
    #[cfg(feature = "revocation")]
    revocation: Option<Arc<crate::auth::revocation::RevocationStore>>,
    /// 刷新令牌轮换 / 重用检测（feature `revocation`）。
    #[cfg(feature = "revocation")]
    rotation: Option<Arc<crate::auth::rotation::RefreshRotation>>,
}

impl AuthService {
    /// 由编解码器装配（**仅 JWT**，不挂 RBAC）。
    #[must_use]
    pub fn new(jwt: JwtCodec) -> Self {
        Self {
            jwt: Arc::new(jwt),
            #[cfg(feature = "authz")]
            authorizer: None,
            #[cfg(feature = "session")]
            sessions: Arc::new(crate::auth::session::MemorySessionStore::new()),
            #[cfg(feature = "revocation")]
            revocation: None,
            #[cfg(feature = "revocation")]
            rotation: None,
        }
    }

    /// 按 `[auth]` 装配（**仅 JWT**）。
    ///
    /// 要 RBAC 请再链一次 [`AuthService::with_authorizer_from_config`]
    /// （feature `authz`）。
    ///
    /// # Errors
    /// 目前不会失败；保留 `Result` 是为了给后续「密钥格式校验」留位置。
    pub async fn from_config(auth: &AuthSection) -> AppResult<Self> {
        Ok(Self::new(JwtCodec::new(&auth.jwt)))
    }

    /// 挂上 RBAC 授权器（feature `authz`）。
    #[cfg(feature = "authz")]
    #[must_use]
    pub fn with_authorizer(mut self, authorizer: Authorizer) -> Self {
        self.authorizer = Some(Arc::new(authorizer));
        self
    }

    /// 按 `[authz]` 装配 RBAC 授权器并挂上（feature `authz`）。
    ///
    /// - `[authz].adapter = file` 且模型文件存在 → 从文件加载
    /// - 否则 → 内嵌默认模型 + 空策略（**默认拒绝一切**）
    ///
    /// # Errors
    /// 模型 / 策略非法时返回错误。
    #[cfg(feature = "authz")]
    pub async fn with_authorizer_from_config(mut self, authz: &AuthzSection) -> AppResult<Self> {
        let authorizer =
            if authz.adapter == AuthzAdapter::File && Path::new(&authz.model_path).is_file() {
                Authorizer::from_files(&authz.model_path, &authz.policy_path).await?
            } else {
                Authorizer::embedded().await?
            };
        self.authorizer = Some(Arc::new(authorizer));
        Ok(self)
    }

    /// JWT 编解码器。
    #[must_use]
    pub fn jwt(&self) -> &JwtCodec {
        &self.jwt
    }

    /// RBAC 授权器（feature `authz`）。
    ///
    /// `None` 表示只装了 JWT、没装策略引擎。此时 [`AuthService::enforce`]
    /// 与 [`AuthService::require`] **一律拒绝**。
    #[cfg(feature = "authz")]
    #[must_use]
    pub fn authorizer(&self) -> Option<&Authorizer> {
        self.authorizer.as_deref()
    }

    /// 会话存储（feature `session`）。
    #[cfg(feature = "session")]
    #[must_use]
    pub fn sessions(&self) -> &Arc<dyn SessionStore> {
        &self.sessions
    }

    /// 覆盖会话存储（如接入 Redis）。
    #[cfg(feature = "session")]
    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.sessions = store;
        self
    }

    /// jti 黑名单存储（feature `revocation`）。
    #[cfg(feature = "revocation")]
    #[must_use]
    pub fn revocation(&self) -> Option<&Arc<crate::auth::revocation::RevocationStore>> {
        self.revocation.as_ref()
    }

    /// 挂载 jti 黑名单（`[auth].revocation.enabled = true` 时由 builder 注入）。
    #[cfg(feature = "revocation")]
    pub fn with_revocation(mut self, store: Arc<crate::auth::revocation::RevocationStore>) -> Self {
        self.revocation = Some(store);
        self
    }

    /// 刷新令牌轮换 / 重用检测存储（feature `revocation`）。
    #[cfg(feature = "revocation")]
    #[must_use]
    pub fn rotation(&self) -> Option<&Arc<crate::auth::rotation::RefreshRotation>> {
        self.rotation.as_ref()
    }

    /// 挂载刷新令牌轮换存储（`[auth].revocation.enabled = true` 时由 builder 注入）。
    #[cfg(feature = "revocation")]
    pub fn with_rotation(mut self, store: Arc<crate::auth::rotation::RefreshRotation>) -> Self {
        self.rotation = Some(store);
        self
    }

    /// 签发 access + refresh 双令牌。
    ///
    /// # Errors
    /// 密钥为空或编码失败时返回错误。
    pub fn sign_jwt(&self, principal: &Principal) -> AppResult<TokenPair> {
        self.jwt.sign_token_pair(principal)
    }

    /// 校验 access 令牌。
    ///
    /// # Errors
    /// 过期 → `20003`；无效 → `20001`。
    pub fn verify_access(&self, token: &str) -> AppResult<Claims> {
        self.jwt.verify_access(token)
    }

    /// 校验 refresh 令牌。
    ///
    /// # Errors
    /// 过期 → `20003`；无效 → `20001`。
    pub fn verify_refresh(&self, token: &str) -> AppResult<Claims> {
        self.jwt.verify_refresh(token)
    }

    /// 用 refresh 令牌换一对新令牌，并**作废旧的那条**（feature `revocation`）。
    ///
    /// 这是刷新端点应该调的方法，不是 [`AuthService::verify_refresh`] +
    /// [`AuthService::sign_jwt`]——后者不会作废旧令牌，泄漏后无法止损。
    ///
    /// 行为：
    /// 1. 验签 / 过期 / 类型不符 → 直接拒绝；
    /// 2. 家族已被拉黑 → 拒绝（不再签发）；
    /// 3. **原子**占用旧 `jti`；占用失败 = 这条 refresh 之前用过 → 判定泄漏 →
    ///    拉黑整条家族 → 拒绝；
    /// 4. 通过 → 用**同一个 `fid`** 签发新的一对（家族延续，泄漏检测才能覆盖后续）。
    ///
    /// 未装配轮换存储时退化为「验签通过就重新签发」，与旧行为一致（仅用于
    /// 没开 `[auth].revocation.enabled` 的场景）。
    ///
    /// 注意：家族拉黑**不会**让已签发的 access 令牌立刻失效，它们由
    /// `access_ttl`（默认 15 分钟）兜底。
    ///
    /// # Errors
    /// 令牌无效 / 过期 / 重放 → `20001` / `20003`；存储不可用且 `fail_closed` → `50004`。
    #[cfg(feature = "revocation")]
    pub async fn rotate_refresh(&self, refresh_token: &str) -> AppResult<TokenPair> {
        let claims = self.verify_refresh(refresh_token)?;
        let principal = claims.principal();
        // 旧令牌没有 `fid` 时按 `sub` 归族：宁可多吊销这个用户的全部会话，
        // 也不要漏掉一次泄漏。
        let family = if claims.fid.is_empty() {
            claims.sub.clone()
        } else {
            claims.fid.clone()
        };

        let Some(rotation) = self.rotation.as_deref() else {
            return self.jwt.sign_token_pair_with_family(&principal, &family);
        };

        // 1) 家族已拉黑：直接拒绝，别再签发
        match rotation.is_family_revoked(&family).await {
            Ok(true) => {
                return Err(crate::errors::AppError::biz(
                    crate::errors::ErrorCode::UNAUTHENTICATED,
                    "该会话已因令牌泄漏被吊销，请重新登录",
                ));
            }
            Ok(false) => {}
            Err(error) => {
                if rotation.fail_closed() {
                    return Err(error);
                }
                tracing::warn!(%error, "刷新令牌家族状态读取失败：按放行处理");
            }
        }

        // 2) 原子占用旧 jti。false = 这条 refresh 之前已经被用过。
        let now = usize::try_from(crate::utils::now_unix_secs()).unwrap_or(0);
        let remaining = claims.exp.saturating_sub(now);
        let ttl = std::time::Duration::from_secs(u64::try_from(remaining).unwrap_or(0));
        match rotation.claim(&claims.jti, ttl).await {
            Ok(true) => {}
            Ok(false) => {
                // 泄漏：整条家族一起拉黑（用户和攻击者都下线，用户重新登录即可）
                if let Err(error) = rotation
                    .revoke_family(&family, self.jwt.refresh_ttl())
                    .await
                {
                    tracing::error!(%error, "拉黑刷新令牌家族失败（泄漏已判定，但状态没写进去）");
                }
                tracing::error!(
                    sub = %claims.sub,
                    family = %family,
                    "刷新令牌被重复使用：判定为泄漏，已吊销整条家族"
                );
                return Err(crate::errors::AppError::biz(
                    crate::errors::ErrorCode::UNAUTHENTICATED,
                    "刷新令牌已被使用过：会话已吊销，请重新登录",
                ));
            }
            Err(error) => {
                if rotation.fail_closed() {
                    return Err(error);
                }
                tracing::warn!(%error, "刷新令牌占用失败：按放行处理（无法保证一次性）");
            }
        }

        // 3) 同一家族内签发新的一对
        self.jwt.sign_token_pair_with_family(&principal, &family)
    }

    /// 判定主体是否可对 `object` 执行 `action`（feature `authz`）。
    ///
    /// 未装配授权器时返回 `false`——没有策略引擎不等于全部放行。
    ///
    /// # Errors
    /// 策略引擎出错时返回错误。
    #[cfg(feature = "authz")]
    pub async fn enforce(
        &self,
        principal: &Principal,
        object: &str,
        action: &str,
    ) -> AppResult<bool> {
        let Some(authorizer) = self.authorizer.as_deref() else {
            tracing::warn!(
                object,
                action,
                "authz 未装配（from_config 之后没调 with_authorizer_from_config）：按拒绝处理"
            );
            return Ok(false);
        };
        authorizer.enforce(principal, object, action).await
    }

    /// 权限判定并转换为错误（供 [`require_permission!`](crate::require_permission) 使用，
    /// feature `authz`）。
    ///
    /// # Errors
    /// 无权时返回 `20004`（403）。
    #[cfg(feature = "authz")]
    pub async fn require(
        &self,
        principal: &Principal,
        object: &str,
        action: &str,
    ) -> AppResult<()> {
        let Some(authorizer) = self.authorizer.as_deref() else {
            tracing::warn!(
                object,
                action,
                "authz 未装配（from_config 之后没调 with_authorizer_from_config）：按拒绝处理"
            );
            return Err(crate::errors::AppError::biz(
                crate::errors::ErrorCode::PERMISSION_DENIED,
                "permission denied",
            ));
        };
        authorizer.require(principal, object, action).await
    }
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("AuthService");
        out.field("jwt", &"<jwt>");
        #[cfg(feature = "authz")]
        out.field("authorizer", &self.authorizer);
        out.finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sections::auth::JwtSection;

    async fn service() -> AuthService {
        let auth = AuthSection {
            jwt: JwtSection {
                secret: "test-secret".to_string(),
                ..JwtSection::default()
            },
            ..AuthSection::default()
        };
        #[allow(unused_mut)]
        let mut service = AuthService::from_config(&auth).await.unwrap();
        #[cfg(feature = "authz")]
        {
            service = service
                .with_authorizer_from_config(&crate::config::AuthzSection::default())
                .await
                .unwrap();
        }
        service
    }

    #[tokio::test]
    async fn should_sign_and_verify_token_pair() {
        let service = service().await;
        let principal = Principal::from_id(1).with_role("admin");
        let pair = service.sign_jwt(&principal).unwrap();

        let claims = service.verify_access(&pair.access_token).unwrap();
        assert_eq!(claims.sub, "1");
        assert!(claims.principal().has_role("admin"));
    }

    #[cfg(feature = "authz")]
    #[tokio::test]
    async fn should_deny_permission_by_default() {
        let service = service().await;
        let allowed = service
            .enforce(&Principal::from_id(1), "users", "delete")
            .await
            .unwrap();
        assert!(!allowed, "默认应拒绝");

        let err = service
            .require(&Principal::from_id(1), "users", "delete")
            .await
            .unwrap_err();
        assert_eq!(err.code(), crate::errors::ErrorCode::PERMISSION_DENIED);
    }

    /// 没装配授权器时也必须**拒绝**，而不是「没有策略引擎 = 全部放行」。
    #[cfg(feature = "authz")]
    #[tokio::test]
    async fn should_deny_when_authorizer_not_wired() {
        let auth = AuthSection {
            jwt: JwtSection {
                secret: "test-secret".to_string(),
                ..JwtSection::default()
            },
            ..AuthSection::default()
        };
        let service = AuthService::from_config(&auth).await.unwrap();
        assert!(service.authorizer().is_none());

        assert!(
            !service
                .enforce(&Principal::from_id(1), "users", "read")
                .await
                .unwrap()
        );
        assert_eq!(
            service
                .require(&Principal::from_id(1), "users", "read")
                .await
                .unwrap_err()
                .code(),
            crate::errors::ErrorCode::PERMISSION_DENIED
        );
    }
}
