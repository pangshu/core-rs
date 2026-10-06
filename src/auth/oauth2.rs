//! OAuth2 授权码 + PKCE 第三方登录（feature = "oauth2"，文档 三·13）。
//!
//! 本模块是「换取身份」的客户端：GitHub / 微信 / 企业 SSO 等，回调换取
//! 用户信息后由应用落成 session / jwt，仍落到统一 [`Identity`](crate::auth::Identity)。
//! 它不参与请求认证链（`[auth].mode` 不含 oauth2）。
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # async fn demo(oauth: &core_rs::auth::oauth2::OAuth2ProviderClient) -> AppResult<()> {
//! // 1) 跳转：authorize_url(state, pkce_verifier)
//! // 2) 回调：exchange_code(code, verifier) 换 token；userinfo 拉用户信息
//! let token = oauth.exchange_code("code-from-provider", "pkce-verifier").await?;
//! let userinfo = oauth.userinfo(&token.access_token()).await?;
//! // 3) userinfo JSON → Identity（sub 取 id 字段）→ 落 session / jwt
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use oauth2::basic::{BasicClient, BasicTokenResponse, BasicTokenType};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    RedirectUrl, Scope, TokenResponse, TokenUrl,
};

use crate::config::sections::OAuth2Provider;
use crate::error::{AppError, AppResult};

/// 单个 provider 的 OAuth2 客户端
pub struct OAuth2ProviderClient {
    settings: OAuth2Provider,
    redirect_url: String,
    client: BasicClient,
}

impl OAuth2ProviderClient {
    pub fn new(settings: OAuth2Provider, redirect_url: String) -> AppResult<Self> {
        if settings.auth_url.is_empty() || settings.token_url.is_empty() {
            return Err(AppError::internal(
                "oauth2 provider missing auth_url / token_url",
            ));
        }
        let auth_url = AuthUrl::new(settings.auth_url.clone())
            .map_err(|e| AppError::internal(format!("bad auth_url: {e}")))?;
        let token_url = TokenUrl::new(settings.token_url.clone())
            .map_err(|e| AppError::internal(format!("bad token_url: {e}")))?;
        let client = BasicClient::new(
            ClientId::new(settings.client_id.clone()),
            Some(ClientSecret::new(settings.client_secret.clone())),
            auth_url,
            Some(token_url),
        )
        .set_redirect_uri(RedirectUrl::new(redirect_url.clone())
            .map_err(|e| AppError::internal(format!("bad redirect_url: {e}")))?);
        Ok(Self {
            settings,
            redirect_url,
            client,
        })
    }

    pub fn provider_redirect_url(&self) -> &str {
        &self.redirect_url
    }

    /// 生成授权跳转地址：返回 `(url, state, pkce_verifier)`。
    /// state 与 verifier 必须暂存（session / 短 TTL cache），回调时校验。
    pub fn authorize_url(&self) -> (String, CsrfToken, PkceCodeVerifier) {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = self
            .client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(challenge);
        for scope in &self.settings.scopes {
            req = req.add_scope(Scope::new(scope.clone()));
        }
        let (url, state) = req.url();
        (
            url.to_string(),
            state,
            PkceCodeVerifier(oauth2::PkceCodeVerifier::new(verifier.secret().to_string())),
        )
    }

    /// 用回调 code 换 access token（PKCE verifier 与发起时配对）
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: PkceCodeVerifier,
    ) -> AppResult<oauth2::basic::BasicTokenResponse> {
        self.client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(verifier.0)
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .map_err(|e| AppError::unauthorized(format!("oauth2 token exchange failed: {e}")))
    }

    /// 拉取用户信息（返回原始 JSON，由应用映射为 Identity claims）
    pub async fn userinfo(&self, access_token: &str) -> AppResult<serde_json::Value> {
        if self.settings.userinfo_url.is_empty() {
            return Err(AppError::internal("oauth2 provider missing userinfo_url"));
        }
        let resp = reqwest::Client::new()
            .get(&self.settings.userinfo_url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| AppError::internal(format!("oauth2 userinfo failed: {e}")))?;
        resp.error_for_status()
            .map_err(|e| AppError::unauthorized(format!("oauth2 userinfo rejected: {e}")))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| AppError::internal(format!("oauth2 userinfo parse failed: {e}")))
    }

    /// token 类型（多数 provider 为 Bearer）
    pub fn token_type(token: &BasicTokenResponse) -> String {
        match token.token_type() {
            BasicTokenType::Bearer => "Bearer".to_string(),
            _ => format!("{:?}", token.token_type()),
        }
    }
}

/// 所有已启用 provider 的注册表（按 `[auth.oauth2].enabled_providers` 装配）
pub struct OAuth2Registry {
    providers: std::collections::BTreeMap<String, Arc<OAuth2ProviderClient>>,
}

impl OAuth2Registry {
    pub fn build(
        settings: &crate::config::sections::OAuth2Settings,
    ) -> AppResult<Option<Self>> {
        let mut providers = std::collections::BTreeMap::new();
        for name in &settings.enabled_providers {
            let Some(p) = settings.providers.get(name) else {
                return Err(AppError::internal(format!(
                    "oauth2 provider {name:?} enabled but missing in [auth.oauth2.providers]"
                )));
            };
            let redirect = settings.redirect_url.replace("{provider}", name);
            providers.insert(
                name.clone(),
                Arc::new(OAuth2ProviderClient::new(p.clone(), redirect)?),
            );
        }
        if providers.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { providers }))
    }

    pub fn get(&self, name: &str) -> Option<Arc<OAuth2ProviderClient>> {
        self.providers.get(name).cloned()
    }
}

/// PKCE verifier 的 newtype（发起与回调之间需由应用暂存）
pub struct PkceCodeVerifier(pub oauth2::PkceCodeVerifier);

// 引用 Identity 避免 unused（模块文档示例使用）
#[allow(unused)]
type _Id = crate::auth::Identity;
