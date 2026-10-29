//! CSRF 中间件（feature `csrf`）。
//!
//! **双提交 Cookie**（double submit cookie）：服务端下发一个 token 到 Cookie，
//! 前端读取后放进请求头 `X-CSRF-Token`；两者一致才放行。
//! 攻击者站点受同源策略限制，**读不到** Cookie 值，也就伪造不出匹配的头。
//!
//! # 为什么不是"朴素双提交"
//!
//! 朴素实现（Cookie 里放一个随机串，请求头里比对同一个串）有三个洞：
//!
//! 1. **可被写入**：同主域的兄弟子域（`evil.example.com`）能给 `example.com`
//!    写 Cookie（cookie tossing）。攻击者写入一个自己知道的值，再让受害者带着
//!    它发请求——若服务端只做"cookie == header"，这次伪造就成立了。
//!    → 这里给 token **加 HMAC 签名**，攻击者写不出合法签名。
//! 2. **永不过期**：token 一旦泄露就一直可用。
//!    → 这里带 `iat` + TTL（`[http].csrf.ttl`）。
//! 3. **不绑会话**：登录前拿到的 token 在登录后照样有效（会话固定类问题）。
//!    → 这里把**会话 Cookie 的值**纳入签名；会话轮换后旧 token 自动失效。
//!
//! # 行为
//!
//! - 安全方法（`GET` / `HEAD` / `OPTIONS`）：放行；token 缺失 / 过期 / 签名不符
//!   （例如刚登录、会话 ID 已轮换）时**重新下发**——否则用户登录后第一次写请求必然 403。
//! - 其余方法：`Cookie` 与 `X-CSRF-Token` 必须同时存在且**签名校验通过**，否则 `20002`（403）。
//! - Cookie **不设 `HttpOnly`**（前端必须能读）；`SameSite=Lax` + `Path=/`
//!   + `Secure`（`[http].csrf.secure`，默认开启；生产环境关闭会拒绝启动）。
//!
//! `TRACE` **不在**安全方法之列：它没有任何正当用途，且是 Cross-Site Tracing 的
//! 前置条件；放行它等于给「读走请求头里的凭证」留一条路。
//!
//! # 密钥
//!
//! `[http].csrf.secret`（敏感项，只走环境变量）用于签名。**多实例部署必须配置**
//! 同一个值，否则 A 实例签的 token 在 B 实例上校验失败。未配置时使用**每进程随机**
//! 密钥并在启动期告警（单实例可用；也仍然防住了"兄弟子域写 Cookie"）。
//!
//! 仅 Cookie 会话场景需要开启；纯 Bearer API 不需要（见 `docs/11` §5）。

use axum::extract::Request;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::CsrfSection;
use crate::errors::{AppError, ErrorCode};
use crate::utils::Secret;
use crate::utils::constant_time_eq;

/// 双提交时前端回传 token 的请求头。
pub const CSRF_HEADER: &str = "x-csrf-token";

/// 允许的时钟偏差（秒）：`iat` 落在未来不超过它。
const MAX_CLOCK_SKEW: u64 = 60;

/// CSRF 编解码器（启动期由 `[http].csrf` 构造一次）。
///
/// 单独成结构体而不是每次请求读配置：签名密钥不该在请求路径上反复克隆。
#[derive(Clone)]
pub(crate) struct CsrfCodec {
    key: Secret<Vec<u8>>,
    ttl: std::time::Duration,
    cookie_name: String,
    /// 会话 Cookie 名（用于把 token 绑到会话上）。
    session_cookie: String,
    secure: bool,
}

impl CsrfCodec {
    /// 由配置构造；密钥为空时使用每进程随机值（多实例需显式配置）。
    pub(crate) fn from_section(section: &CsrfSection) -> Self {
        let key = if section.secret.trim().is_empty() {
            // 32 字节随机：足以防伪造，但**不跨实例**，故告警
            let mut key = Vec::with_capacity(32);
            key.extend_from_slice(Uuid::new_v4().as_bytes());
            key.extend_from_slice(Uuid::new_v4().as_bytes());
            tracing::warn!(
                "[http].csrf.secret 未配置：使用每进程随机密钥，CSRF token 不跨实例有效。\
                 多实例部署请配置 AURORA_HTTP__CSRF__SECRET（同一值）"
            );
            key
        } else {
            section.secret.as_bytes().to_vec()
        };

        Self {
            key: Secret::new(key),
            ttl: section.ttl,
            cookie_name: section.cookie_name.trim().to_string(),
            session_cookie: section.session_cookie.trim().to_string(),
            secure: section.secure,
        }
    }

    /// 生成 token：`nonce.iat.mac`。
    ///
    /// `mac = HMAC(key, sid || 0 || nonce || 0 || iat)`：会话 ID 参与签名，
    /// 因此会话轮换（登录）后旧 token 自然失效。
    fn issue(&self, sid: &str) -> String {
        let nonce = Uuid::new_v4().simple().to_string();
        let iat = crate::utils::now_unix_secs().max(0) as u64;
        let mac = self.mac(sid, &nonce, iat);
        format!("{nonce}.{iat}.{mac}")
    }

    /// 校验 token（签名 + TTL + 会话绑定）。
    fn verify(&self, sid: &str, token: &str) -> bool {
        let mut parts = token.split('.');
        let (Some(nonce), Some(iat), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        if nonce.is_empty() || mac.is_empty() {
            return false;
        }
        let Ok(iat) = iat.parse::<u64>() else {
            return false;
        };

        let now = crate::utils::now_unix_secs().max(0) as u64;
        // 过期（或来自未来的离谱时间）
        if now.saturating_sub(iat) > self.ttl.as_secs() || iat > now.saturating_add(MAX_CLOCK_SKEW)
        {
            return false;
        }

        constant_time_eq(self.mac(sid, nonce, iat).as_bytes(), mac.as_bytes())
    }

    /// 计算签名（十六进制）。
    fn mac(&self, sid: &str, nonce: &str, iat: u64) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.key.expose())
            .expect("HMAC 接受任意长度密钥");
        mac.update(sid.as_bytes());
        mac.update(&[0]);
        mac.update(nonce.as_bytes());
        mac.update(&[0]);
        mac.update(iat.to_string().as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// 下发 token Cookie（`HttpOnly` 必须为假：前端要读出来放进请求头）。
    fn set_cookie(&self, response: &mut Response, sid: &str) {
        let token = self.issue(sid);
        let mut cookie = format!(
            "{}={token}; Path=/; SameSite=Lax; Max-Age={}",
            self.cookie_name,
            self.ttl.as_secs()
        );
        if self.secure {
            cookie.push_str("; Secure");
        }
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
}

/// 校验（或下发）CSRF token；`enabled = false` 时直接透传。
pub(crate) async fn handle(codec: CsrfCodec, req: Request, next: Next) -> Response {
    let sid = cookie_value(req.headers(), &codec.session_cookie).unwrap_or_default();
    let presented = cookie_value(req.headers(), &codec.cookie_name);

    if is_safe_method(req.method()) {
        // 安全方法：token 缺失 / 过期 / 签名不符（会话已轮换）都重新下发
        let valid = presented
            .as_deref()
            .is_some_and(|token| codec.verify(&sid, token));
        let mut response = next.run(req).await;
        if !valid {
            codec.set_cookie(&mut response, &sid);
        }
        return response;
    }

    let header = req
        .headers()
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    match (presented, header) {
        (Some(cookie), Some(header))
            if constant_time_eq(cookie.as_bytes(), header.as_bytes())
                && codec.verify(&sid, &cookie) =>
        {
            next.run(req).await
        }
        _ => {
            tracing::warn!(
                path = %req.uri().path(),
                has_cookie = req.headers().get(COOKIE).is_some(),
                has_header = req.headers().get(CSRF_HEADER).is_some(),
                "CSRF 校验失败"
            );
            AppError::biz(
                ErrorCode::FORBIDDEN,
                format!("CSRF 校验失败：缺少或无效的 {CSRF_HEADER}（请先 GET 一次以获取新 token）"),
            )
            .into_response()
        }
    }
}

/// 是否为安全方法（不改状态，无需校验）。
///
/// `TRACE` 故意不在其中：它没有正当用途，且是 Cross-Site Tracing 的前置条件
/// （部分服务器会把请求头回显进响应体，攻击者据此读走 Cookie）。
fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// 从 `Cookie` 头中取出指定名字的值。
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
        .find(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;

    fn section() -> CsrfSection {
        CsrfSection {
            enabled: true,
            cookie_name: "csrf_token".to_string(),
            secure: true,
            ttl: Duration::from_secs(600),
            secret: "csrf-test-secret-0123456789abcdef".to_string(),
            session_cookie: "sid".to_string(),
        }
    }

    fn app() -> Router {
        let codec = CsrfCodec::from_section(&section());
        Router::new()
            .route("/read", get(|| async { "ok" }))
            .route("/write", post(|| async { "ok" }))
            .layer(from_fn(move |req: Request, next: Next| {
                let codec = codec.clone();
                async move { handle(codec, req, next).await }
            }))
    }

    async fn send(app: Router, method: Method, path: &str, headers: &[(&str, &str)]) -> Response {
        let mut builder = HttpRequest::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app.oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn set_cookie_of(response: &Response) -> String {
        response
            .headers()
            .get(SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// 从 `Set-Cookie` 里取出 token 值。
    fn token_of(cookie: &str) -> String {
        cookie
            .split(';')
            .next()
            .and_then(|pair| pair.split_once('='))
            .map(|(_, value)| value.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn should_issue_signed_cookie_on_safe_request() {
        let response = send(app(), Method::GET, "/read", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);

        let cookie = set_cookie_of(&response);
        assert!(cookie.starts_with("csrf_token="), "实际：{cookie}");
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"), "默认必须带 Secure：{cookie}");
        assert!(!cookie.contains("HttpOnly"), "前端必须能读到 token");
        assert!(cookie.contains("Max-Age=600"), "必须带 TTL：{cookie}");

        // token 形状：nonce.iat.mac
        let token = token_of(&cookie);
        assert_eq!(token.split('.').count(), 3, "token 应带签名：{token}");
    }

    #[tokio::test]
    async fn should_reject_write_without_token() {
        let response = send(app(), Method::POST, "/write", &[]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn should_reject_unsigned_or_forged_token() {
        // 攻击者（兄弟子域）能写 Cookie，但写不出合法签名
        let forged = "deadbeef.1700000000.0123456789abcdef";
        let response = send(
            app(),
            Method::POST,
            "/write",
            &[
                ("cookie", &format!("csrf_token={forged}")),
                (CSRF_HEADER, forged),
            ],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "无签名 / 伪造签名必须拒绝（朴素双提交在这里会被绕过）"
        );
    }

    #[tokio::test]
    async fn should_accept_valid_token() {
        let codec = CsrfCodec::from_section(&section());
        let token = codec.issue("sid-1");

        let response = send(
            app(),
            Method::POST,
            "/write",
            &[
                ("cookie", &format!("sid=sid-1; csrf_token={token}")),
                (CSRF_HEADER, &token),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn should_reject_token_bound_to_another_session() {
        let codec = CsrfCodec::from_section(&section());
        let token = codec.issue("sid-old");

        // 会话已轮换（登录），旧 token 绑的是旧会话 → 拒绝
        let response = send(
            app(),
            Method::POST,
            "/write",
            &[
                ("cookie", &format!("sid=sid-new; csrf_token={token}")),
                (CSRF_HEADER, &token),
            ],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "token 必须绑定会话，登录后旧 token 应失效"
        );
    }

    #[tokio::test]
    async fn should_reissue_when_session_rotated() {
        let codec = CsrfCodec::from_section(&section());
        let stale = codec.issue("sid-old");

        // 会话轮换后访问安全方法：应自动换发新 token（而不是让用户卡在 403）
        let response = send(
            app(),
            Method::GET,
            "/read",
            &[("cookie", &format!("sid=sid-new; csrf_token={stale}"))],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = set_cookie_of(&response);
        assert!(
            cookie.starts_with("csrf_token="),
            "会话轮换后应重新下发 token：{cookie}"
        );
        assert_ne!(token_of(&cookie), stale);
    }

    #[test]
    fn should_reject_expired_token() {
        let codec = CsrfCodec {
            ttl: Duration::from_secs(60),
            ..CsrfCodec::from_section(&section())
        };
        // 手工构造一个"1 小时前签发"的合法签名：签名对、但超过 TTL
        let nonce = "0123456789abcdef0123456789abcdef";
        let iat = (crate::utils::now_unix_secs().max(0) as u64).saturating_sub(3600);
        let mac = codec.mac("", nonce, iat);
        assert!(
            !codec.verify("", &format!("{nonce}.{iat}.{mac}")),
            "超过 TTL 必须拒绝"
        );

        // 对照：同一时刻签发的仍然有效
        let fresh = codec.issue("");
        assert!(codec.verify("", &fresh));
    }

    #[tokio::test]
    async fn should_not_treat_trace_as_safe_method() {
        let response = send(app(), Method::TRACE, "/write", &[]).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn should_not_reissue_when_token_still_valid() {
        let codec = CsrfCodec::from_section(&section());
        let token = codec.issue("sid-1");
        let response = send(
            app(),
            Method::GET,
            "/read",
            &[("cookie", &format!("sid=sid-1; csrf_token={token}"))],
        )
        .await;
        assert!(response.headers().get(SET_COOKIE).is_none());
    }
}
