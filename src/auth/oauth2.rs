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

use std::sync::Arc; // 引入原子引用计数指针，跨请求共享 provider 客户端

use oauth2::basic::{BasicClient, BasicTokenResponse, BasicTokenType}; // 引入 oauth2 基础客户端与 token 类型
use oauth2::{ // 引入 oauth2 核心类型
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, // 授权地址/授权码/客户端凭据/CSRF/PKCE 挑战
    RedirectUrl, Scope, TokenResponse, TokenUrl, // 回调地址/作用域/token 响应/token 地址
};

use crate::config::sections::OAuth2Provider; // 引入单个 provider 的配置结构
use crate::error::{AppError, AppResult}; // 引入框架错误类型与结果别名

/// 单个 provider 的 OAuth2 客户端
pub struct OAuth2ProviderClient { // 定义单个 provider 的客户端
    settings: OAuth2Provider, // provider 配置（URL、client 凭据、scopes 等）
    redirect_url: String, // 已解析的回调地址（{provider} 已替换）
    client: BasicClient, // oauth2 库的基础客户端
}

impl OAuth2ProviderClient { // 为 provider 客户端提供构造与流程方法
    pub fn new(settings: OAuth2Provider, redirect_url: String) -> AppResult<Self> { // 由配置与回调地址构造
        if settings.auth_url.is_empty() || settings.token_url.is_empty() { // 缺少必要 URL 则无法工作
            return Err(AppError::internal( // 返回内部错误
                "oauth2 provider missing auth_url / token_url", // 提示缺少授权/令牌地址
            ));
        }
        let auth_url = AuthUrl::new(settings.auth_url.clone()) // 解析授权地址
            .map_err(|e| AppError::internal(format!("bad auth_url: {e}")))?; // 非法则返回内部错误
        let token_url = TokenUrl::new(settings.token_url.clone()) // 解析令牌地址
            .map_err(|e| AppError::internal(format!("bad token_url: {e}")))?; // 非法则返回内部错误
        let client = BasicClient::new( // 构造基础客户端
            ClientId::new(settings.client_id.clone()), // 客户端 id
            Some(ClientSecret::new(settings.client_secret.clone())), // 客户端密钥
            auth_url, // 授权地址
            Some(token_url), // 令牌地址
        )
        .set_redirect_uri(RedirectUrl::new(redirect_url.clone()) // 解析并设置回调地址
            .map_err(|e| AppError::internal(format!("bad redirect_url: {e}")))?); // 非法则返回内部错误
        Ok(Self { // 组装客户端
            settings, // 保存 provider 配置
            redirect_url, // 保存回调地址
            client, // 保存基础客户端
        })
    }

    pub fn provider_redirect_url(&self) -> &str { // 暴露回调地址
        &self.redirect_url // 返回内部引用
    }

    /// 生成授权跳转地址：返回 `(url, state, pkce_verifier)`。
    /// state 与 verifier 必须暂存（session / 短 TTL cache），回调时校验。
    pub fn authorize_url(&self) -> (String, CsrfToken, PkceCodeVerifier) { // 生成授权跳转所需数据
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256(); // 生成 PKCE 挑战与配对的 verifier
        let mut req = self // 基于客户端构造授权请求
            .client // 使用基础客户端
            .authorize_url(CsrfToken::new_random) // 生成随机 state 防 CSRF
            .set_pkce_challenge(challenge); // 设置 PKCE 挑战
        for scope in &self.settings.scopes { // 遍历配置的作用域
            req = req.add_scope(Scope::new(scope.clone())); // 逐个加入授权范围
        }
        let (url, state) = req.url(); // 生成最终授权 URL 与 state
        (
            url.to_string(), // 授权跳转地址
            state, // CSRF state，需暂存
            PkceCodeVerifier(oauth2::PkceCodeVerifier::new(verifier.secret().to_string())), // 包装 verifier，需暂存
        )
    }

    /// 用回调 code 换 access token（PKCE verifier 与发起时配对）
    pub async fn exchange_code( // 授权码换令牌
        &self,
        code: &str, // 回调返回的授权码
        verifier: PkceCodeVerifier, // 发起时生成的 PKCE verifier
    ) -> AppResult<oauth2::basic::BasicTokenResponse> { // 返回令牌响应
        self.client // 使用基础客户端
            .exchange_code(AuthorizationCode::new(code.to_string())) // 设置授权码
            .set_pkce_verifier(verifier.0) // 设置 PKCE verifier 完成校验配对
            .request_async(oauth2::reqwest::async_http_client) // 异步发起 HTTP 请求
            .await
            .map_err(|e| AppError::unauthorized(format!("oauth2 token exchange failed: {e}"))) // 失败映射为 401
    }

    /// 拉取用户信息（返回原始 JSON，由应用映射为 Identity claims）
    pub async fn userinfo(&self, access_token: &str) -> AppResult<serde_json::Value> { // 用令牌拉取用户信息
        if self.settings.userinfo_url.is_empty() { // 未配置用户信息地址
            return Err(AppError::internal("oauth2 provider missing userinfo_url")); // 返回内部错误
        }
        let resp = reqwest::Client::new() // 新建 HTTP 客户端
            .get(&self.settings.userinfo_url) // 请求用户信息地址
            .bearer_auth(access_token) // 以 Bearer 令牌鉴权
            .send() // 发送请求
            .await
            .map_err(|e| AppError::internal(format!("oauth2 userinfo failed: {e}")))?; // 网络失败返回内部错误
        resp.error_for_status() // 非 2xx 转错误
            .map_err(|e| AppError::unauthorized(format!("oauth2 userinfo rejected: {e}")))? // 被拒映射为 401
            .json::<serde_json::Value>() // 解析响应体为 JSON
            .await
            .map_err(|e| AppError::internal(format!("oauth2 userinfo parse failed: {e}"))) // 解析失败返回内部错误
    }

    /// token 类型（多数 provider 为 Bearer）
    pub fn token_type(token: &BasicTokenResponse) -> String { // 规范化 token 类型字符串
        match token.token_type() { // 匹配库返回的类型
            BasicTokenType::Bearer => "Bearer".to_string(), // 标准 Bearer
            _ => format!("{:?}", token.token_type()), // 其余类型以调试格式输出
        }
    }
}

/// 所有已启用 provider 的注册表（按 `[auth.oauth2].enabled_providers` 装配）
pub struct OAuth2Registry { // 定义 provider 注册表
    providers: std::collections::BTreeMap<String, Arc<OAuth2ProviderClient>>, // 名称到客户端的映射（有序便于稳定遍历）
}

impl OAuth2Registry { // 为注册表提供装配与查询
    pub fn build( // 按配置装配所有已启用 provider
        settings: &crate::config::sections::OAuth2Settings, // OAuth2 全局配置
    ) -> AppResult<Option<Self>> { // 无启用 provider 时返回 None
        let mut providers = std::collections::BTreeMap::new(); // 初始化映射
        for name in &settings.enabled_providers { // 遍历启用的 provider 名
            let Some(p) = settings.providers.get(name) else { // 查找对应配置
                return Err(AppError::internal(format!( // 启用却无配置则报错
                    "oauth2 provider {name:?} enabled but missing in [auth.oauth2.providers]" // 提示缺失配置
                )));
            };
            let redirect = settings.redirect_url.replace("{provider}", name); // 用 provider 名替换回调模板
            providers.insert( // 插入注册表
                name.clone(), // provider 名
                Arc::new(OAuth2ProviderClient::new(p.clone(), redirect)?), // 构造并共享客户端
            );
        }
        if providers.is_empty() { // 没有任何启用 provider
            return Ok(None); // 返回 None，视为未启用 OAuth2
        }
        Ok(Some(Self { providers })) // 返回注册表
    }

    pub fn get(&self, name: &str) -> Option<Arc<OAuth2ProviderClient>> { // 按名获取 provider 客户端
        self.providers.get(name).cloned() // 克隆 Arc 后返回
    }
}

/// PKCE verifier 的 newtype（发起与回调之间需由应用暂存）
pub struct PkceCodeVerifier(pub oauth2::PkceCodeVerifier); // 包装库类型，便于对外暴露

// 引用 Identity 避免 unused（模块文档示例使用）
#[allow(unused)] // 抑制未使用告警
type _Id = crate::auth::Identity; // 类型别名，仅为让文档示例中的 Identity 链接有效
