//! OCSP Stapling（feature `ocsp-stapling`，`docs/16` §10）。
//!
//! 服务端在 TLS 握手时把**由 CA 签发的 OCSP 响应**一并发给客户端（RFC 6066 §8），
//! 客户端无需再自行访问 CA 的 responder —— 少一次外网请求、不泄露访问行为。
//!
//! 流程：
//!
//! ```text
//! 证书链 → 构造 OCSP 请求（SHA-1 CertID）→ POST/GET responder → 解析 + 校验 → 装订
//!                ↑                                                    │
//!                └──────────── 后台按 refresh_interval 刷新 ←──────────┘
//! ```
//!
//! **校验项**（任一不过就丢弃该响应，继续用旧的/不装订）：
//! 1. `responseStatus == successful`，`responseType == id-pkix-ocsp-basic`
//! 2. 返回的 `CertID` 与本证书一致（serial + issuer name/key hash）
//! 3. `certStatus == good`（`revoked` 直接报错：绝不把吊销状态藏起来）
//! 4. 时间窗口：`thisUpdate` 不在未来、`nextUpdate` 未过期（容忍 `max_skew`）
//! 5. **签名有效**：由 issuer 直接签，或由 issuer 签发的委派 responder 签；
//!    委派 responder 还必须**获授权**——非 CA、`extendedKeyUsage` 含
//!    `id-kp-OCSPSigning`、`keyUsage`（若有）含 `digitalSignature`（RFC 6960 §4.2.2.2）
//!
//! **不做**：完整证书路径校验（以本证书的 issuer 为信任锚，不再往上追到根），
//! OCSP nonce（会让 responder 无法缓存，且多数实现忽略）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use const_oid::AssociatedOid as _;
use der::{Decode, Encode};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;
use sha1::Sha1;
use x509_cert::Certificate;
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{
    AuthorityInfoAccessSyntax, BasicConstraints, ExtendedKeyUsage, KeyUsage,
};
use x509_ocsp::builder::OcspRequestBuilder;
use x509_ocsp::{BasicOcspResponse, CertId, CertStatus, OcspResponse, OcspResponseStatus, Request};

use crate::config::OcspSection;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::tls::cert::now_unix;
use crate::tls::service::StoreHandle;
use crate::tls::store::{CertEntry, CertStore};

/// OCSP 请求的 Content-Type（RFC 6960 A.1.1）。
const REQUEST_CONTENT_TYPE: &str = "application/ocsp-request";
/// OCSP 响应的 Content-Type。
const RESPONSE_CONTENT_TYPE: &str = "application/ocsp-response";
/// 委派 responder 必须具备的扩展密钥用法（RFC 6960 §4.2.2.2）。
const ID_KP_OCSP_SIGNING: const_oid::ObjectIdentifier = const_oid::db::rfc5280::ID_KP_OCSP_SIGNING;

/// 一份已校验、可直接装订的 OCSP 响应。
#[derive(Debug, Clone)]
pub struct Stapled {
    /// 原始 `OCSPResponse` DER（RFC 6066 要求原样装订）。
    pub der: Vec<u8>,
    /// `thisUpdate`（Unix 秒）。
    pub this_update: i64,
    /// `nextUpdate`（Unix 秒；RFC 允许省略）。
    pub next_update: Option<i64>,
}

/// 缓存条目：已装订的证书 + 刷新所需的元数据。
#[derive(Debug)]
struct Cached {
    key: Arc<CertifiedKey>,
    /// 装订时对应的叶子证书：握手前必须与当前仓库里的证书一致，
    /// 否则缓存里那份 `CertifiedKey` 带的是**旧证书链**（热更新后绝不能下发）。
    leaf: CertificateDer<'static>,
    next_update: Option<i64>,
    fetched_at: i64,
}

/// OCSP 装订管理器。
///
/// 持有「域名模式 → 已装订证书」，`SniResolver` 每次握手先查这里，
/// 命中就返回带 `ocsp` 字段的证书；未命中回退到未装订的原始证书。
pub struct OcspManager {
    config: OcspSection,
    client: reqwest::Client,
    cached: RwLock<HashMap<String, Cached>>,
    /// 域名模式 → 上一次失败原因（成功后清除；用于决定是否重试）。
    errors: RwLock<HashMap<String, String>>,
}

impl OcspManager {
    /// 按 `[tls.ocsp]` 构造（不发起网络请求，首轮刷新由 [`spawn`](Self::spawn) 触发）。
    ///
    /// # Errors
    /// HTTP 客户端构建失败时返回 `50005`。
    pub fn new(config: &OcspSection) -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(concat!("aurora-ocsp/", env!("CARGO_PKG_VERSION")))
            // 不跟随重定向：responder 被控或链路被劫持时，3xx 能把请求引向内网地址
            // （云元数据、本机管理端口），响应体签名拦得住「读数据」，拦不住「打点」
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ocsp_error(format!("OCSP HTTP 客户端初始化失败：{error}")))?;

        Ok(Self {
            config: config.clone(),
            client,
            cached: RwLock::new(HashMap::new()),
            errors: RwLock::new(HashMap::new()),
        })
    }

    /// 取已装订的证书（按域名模式，默认证书的模式为空串）。
    ///
    /// 传当前证书记录而不是裸模式串：缓存必须与**当前**证书是同一张，
    /// 否则热更新后会把旧证书链（连带旧 OCSP 响应）继续发给客户端。
    #[must_use]
    pub fn resolve(&self, entry: &CertEntry) -> Option<Arc<CertifiedKey>> {
        let cached = self.cached.read().ok()?;
        let item = cached.get(&entry.pattern)?;
        if entry.key.cert.first() != Some(&item.leaf) {
            return None;
        }
        Some(Arc::clone(&item.key))
    }

    /// 上一次失败的域名与原因（成功后自动清除；观测用）。
    #[must_use]
    pub fn last_errors(&self) -> Vec<(String, String)> {
        self.errors
            .read()
            .map(|errors| {
                errors
                    .iter()
                    .map(|(pattern, error)| (pattern.clone(), error.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 已装订的证书数量（观测用）。
    #[must_use]
    pub fn stapled_count(&self) -> usize {
        self.cached.read().map(|cached| cached.len()).unwrap_or(0)
    }

    /// 清空缓存（证书重载后调用：旧的装订响应对新证书无效）。
    pub fn invalidate(&self) {
        if let Ok(mut cached) = self.cached.write() {
            cached.clear();
        }
        if let Ok(mut errors) = self.errors.write() {
            errors.clear();
        }
    }

    /// 启动后台刷新任务（首个 tick 立即执行，即启动时就会拉一次）。
    ///
    /// 不在 tokio 运行时内时只告警，不 panic。
    pub fn spawn(self: &Arc<Self>, store: StoreHandle) {
        let manager = Arc::clone(self);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("当前不在 tokio 运行时内，OCSP 刷新任务未启动");
            return;
        };

        handle.spawn(async move {
            let mut ticker = tokio::time::interval(manager.config.refresh_interval);
            // 首个 tick 立即返回：启动即拉一次，避免头几秒无响应可装订
            loop {
                ticker.tick().await;
                let snapshot = match store.read() {
                    Ok(guard) => Arc::clone(&guard),
                    Err(poisoned) => Arc::clone(&poisoned.into_inner()),
                };
                manager.refresh(&snapshot).await;
            }
        });
    }

    /// 刷新一轮：只处理「没有缓存 / 快到期 / 上次失败」的证书。
    ///
    /// 单张证书失败不影响其余证书（逐张独立处理）。
    pub async fn refresh(&self, store: &CertStore) {
        let now = now_unix();
        self.prune_expired(now);

        for entry in store.entries() {
            if !self.needs_refresh(entry, now) {
                continue;
            }
            let label = domain_label(&entry.pattern);
            match self.fetch_one(entry).await {
                Ok(stapled) => {
                    let Some(leaf) = entry.key.cert.first().cloned() else {
                        continue;
                    };
                    let key = attach(entry, &stapled);
                    if let Ok(mut cached) = self.cached.write() {
                        cached.insert(
                            entry.pattern.clone(),
                            Cached {
                                key: Arc::new(key),
                                leaf,
                                next_update: stapled.next_update,
                                fetched_at: now,
                            },
                        );
                    }
                    if let Ok(mut errors) = self.errors.write() {
                        errors.remove(&entry.pattern);
                    }
                    tracing::info!(
                        domain = %label,
                        this_update = stapled.this_update,
                        next_update = ?stapled.next_update,
                        bytes = stapled.der.len(),
                        "OCSP 响应已装订"
                    );
                }
                Err(error) => {
                    if let Ok(mut errors) = self.errors.write() {
                        errors.insert(entry.pattern.clone(), error.to_string());
                    }
                    tracing::warn!(
                        domain = %label,
                        %error,
                        "OCSP 响应获取失败（继续使用旧响应；无旧响应则本次握手不装订）"
                    );
                }
            }
        }
    }

    /// 丢弃已过期的装订响应。
    ///
    /// 宁可**不装订**也不装订过期响应：部分客户端对过期的 stapled 响应会直接拒绝连接，
    /// 而「没有装订」只是退化成客户端自行回源。
    fn prune_expired(&self, now: i64) {
        let skew = self.config.max_skew.as_secs() as i64;
        // 响应里没有 `nextUpdate` 时无法证明新鲜度：按「已缓存时长」兜底，
        // 超过提前刷新窗口就丢弃（`needs_refresh` 会立刻重拉，成功则无缝续上）
        let fallback = self.config.refresh_ahead.as_secs() as i64;
        let Ok(mut cached) = self.cached.write() else {
            return;
        };
        cached.retain(|pattern, entry| {
            let deadline = entry
                .next_update
                .unwrap_or(entry.fetched_at.saturating_add(fallback.max(1)));
            let alive = deadline.saturating_add(skew) >= now;
            if !alive {
                tracing::warn!(
                    domain = %domain_label(pattern),
                    "OCSP 响应已过期且未能刷新，暂停装订（客户端将自行回源）"
                );
            }
            alive
        });
    }

    /// 是否需要（重新）拉取。
    fn needs_refresh(&self, entry: &CertEntry, now: i64) -> bool {
        // 上次失败：每轮都重试（responder 抖动恢复后能自愈）
        if self
            .errors
            .read()
            .map(|errors| errors.contains_key(&entry.pattern))
            .unwrap_or(false)
        {
            return true;
        }

        let cached = match self.cached.read() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(item) = cached.get(&entry.pattern) else {
            return true;
        };
        // 缓存对应的是另一张证书（热更新后未清缓存）→ 立即重拉
        if entry.key.cert.first() != Some(&item.leaf) {
            return true;
        }

        let ahead = self.config.refresh_ahead.as_secs() as i64;
        match item.next_update {
            // 进入「提前刷新窗口」才重拉，避免无谓地打 responder
            Some(next_update) => next_update - now <= ahead,
            // 没有 nextUpdate 时按「已缓存时长」兜底
            None => now - item.fetched_at >= ahead.max(1),
        }
    }

    /// 拉取并校验单张证书的 OCSP 响应。
    async fn fetch_one(&self, entry: &CertEntry) -> AppResult<Stapled> {
        let cert_der = entry
            .key
            .cert
            .first()
            .ok_or_else(|| ocsp_error("证书链为空，无法构造 OCSP 请求"))?;
        // 链上第二个是签发者；自签名或只给了叶子证书时无法做 OCSP
        let issuer_der = entry.key.cert.get(1).ok_or_else(|| {
            ocsp_error("证书链只有一张证书，缺少签发者（OCSP 需要 issuer 才能算 CertID）")
        })?;

        let cert = Certificate::from_der(cert_der.as_ref())
            .map_err(|error| ocsp_error(format!("解析叶子证书失败：{error}")))?;
        let issuer = Certificate::from_der(issuer_der.as_ref())
            .map_err(|error| ocsp_error(format!("解析签发者证书失败：{error}")))?;

        let url = self.responder_url(&cert)?;
        let request = build_request(&cert, &issuer)?;
        let body = self.post(&url, &request).await?;
        validate_response(&body, &cert, &issuer, self.config.max_skew)
    }

    /// responder 地址：配置优先，否则取证书 AIA 扩展的 `id-ad-ocsp`。
    fn responder_url(&self, cert: &Certificate) -> AppResult<String> {
        let configured = self.config.responder.trim();
        if !configured.is_empty() {
            return Ok(warn_insecure(configured.to_string()));
        }

        let extensions = cert
            .tbs_certificate
            .extensions
            .as_ref()
            .ok_or_else(|| ocsp_error("证书没有扩展，找不到 AIA（OCSP responder）"))?;

        for extension in extensions {
            if extension.extn_id != AuthorityInfoAccessSyntax::OID {
                continue;
            }
            let aia = AuthorityInfoAccessSyntax::from_der(extension.extn_value.as_bytes())
                .map_err(|error| ocsp_error(format!("解析 AIA 扩展失败：{error}")))?;
            for description in &aia.0 {
                if description.access_method != const_oid::db::rfc5280::ID_AD_OCSP {
                    continue;
                }
                if let GeneralName::UniformResourceIdentifier(uri) = &description.access_location {
                    return Ok(warn_insecure(uri.to_string()));
                }
            }
        }

        Err(ocsp_error(
            "证书里没有 OCSP responder 地址（AIA 无 id-ad-ocsp），请配置 [tls.ocsp].responder",
        ))
    }

    /// POST 请求；responder 不支持 POST（405/501）时回退到 GET。
    async fn post(&self, url: &str, request: &[u8]) -> AppResult<Vec<u8>> {
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, REQUEST_CONTENT_TYPE)
            .header(reqwest::header::ACCEPT, RESPONSE_CONTENT_TYPE)
            .body(request.to_vec())
            .send()
            .await
            .map_err(|error| ocsp_error(format!("请求 OCSP responder 失败（{url}）：{error}")))?;

        let status = response.status();
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            || status == reqwest::StatusCode::NOT_IMPLEMENTED
        {
            tracing::debug!(%url, "responder 不支持 POST，改用 GET");
            return self.get(url, request).await;
        }
        if !status.is_success() {
            return Err(ocsp_error(format!(
                "OCSP responder 返回 HTTP {status}（{url}）"
            )));
        }

        read_body(response, self.config.max_response_bytes).await
    }

    /// GET 形式（RFC 6960 A.1.2）：请求 DER 做 base64 后再 URL 转义。
    async fn get(&self, url: &str, request: &[u8]) -> AppResult<Vec<u8>> {
        let encoded = BASE64.encode(request);
        let escaped = encoded
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D");
        let target = format!("{}/{}", url.trim_end_matches('/'), escaped);

        let response = self
            .client
            .get(&target)
            .header(reqwest::header::ACCEPT, RESPONSE_CONTENT_TYPE)
            .send()
            .await
            .map_err(|error| ocsp_error(format!("GET OCSP responder 失败（{url}）：{error}")))?;

        if !response.status().is_success() {
            return Err(ocsp_error(format!(
                "OCSP responder 返回 HTTP {}（{url}）",
                response.status()
            )));
        }
        read_body(response, self.config.max_response_bytes).await
    }
}

impl std::fmt::Debug for OcspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcspManager")
            .field("responder", &self.config.responder)
            .field("stapled", &self.stapled_count())
            .finish_non_exhaustive()
    }
}

/// 构造 OCSP 请求（DER）。
///
/// 只用 SHA-1 CertID：RFC 6960 §4.1.1 要求 responder **必须**支持 SHA-1，
/// 用 SHA-256 会有一部分 responder 直接返回 `malformedRequest`。
fn build_request(cert: &Certificate, issuer: &Certificate) -> AppResult<Vec<u8>> {
    let request = OcspRequestBuilder::default()
        .with_request(
            Request::from_cert::<Sha1>(issuer, cert)
                .map_err(|error| ocsp_error(format!("构造 OCSP CertID 失败：{error}")))?,
        )
        .build();
    request
        .to_der()
        .map_err(|error| ocsp_error(format!("编码 OCSP 请求失败：{error}")))
}

/// 解析并校验 OCSP 响应。
///
/// # Errors
/// 状态非 successful、CertID 不匹配、证书已吊销、时间窗口越界或验签失败。
fn validate_response(
    body: &[u8],
    cert: &Certificate,
    issuer: &Certificate,
    max_skew: Duration,
) -> AppResult<Stapled> {
    let response = OcspResponse::from_der(body)
        .map_err(|error| ocsp_error(format!("解析 OCSP 响应失败：{error}")))?;

    if response.response_status != OcspResponseStatus::Successful {
        return Err(ocsp_error(format!(
            "OCSP 响应状态为 {:?}（期望 successful）",
            response.response_status
        )));
    }

    let response_bytes = response
        .response_bytes
        .ok_or_else(|| ocsp_error("OCSP 响应缺少 responseBytes"))?;
    if response_bytes.response_type != BasicOcspResponse::OID {
        return Err(ocsp_error(format!(
            "不支持的 OCSP 响应类型 {}（只支持 id-pkix-ocsp-basic）",
            response_bytes.response_type
        )));
    }

    let basic = BasicOcspResponse::from_der(response_bytes.response.as_bytes())
        .map_err(|error| ocsp_error(format!("解析 BasicOCSPResponse 失败：{error}")))?;

    // 1) 找到本证书对应的 SingleResponse
    let expected = CertId::from_issuer::<Sha1>(issuer, cert.tbs_certificate.serial_number.clone())
        .map_err(|error| ocsp_error(format!("重算 CertID 失败：{error}")))?;
    let single = basic
        .tbs_response_data
        .responses
        .iter()
        .find(|single| single.cert_id == expected)
        .ok_or_else(|| ocsp_error("OCSP 响应里没有本证书的 CertID（responder 返回了别的证书）"))?;

    // 2) 状态必须 good
    match &single.cert_status {
        CertStatus::Good(_) => {}
        CertStatus::Revoked(info) => {
            return Err(ocsp_error(format!(
                "证书已被吊销（revocationTime = {}），请立即更换证书",
                info.revocation_time.0.to_unix_duration().as_secs()
            )));
        }
        CertStatus::Unknown(_) => {
            return Err(ocsp_error("OCSP 响应为 unknown（responder 不认识该证书）"));
        }
    }

    // 3) 时间窗口
    let this_update = single.this_update.0.to_unix_duration().as_secs() as i64;
    let next_update = single
        .next_update
        .as_ref()
        .map(|time| time.0.to_unix_duration().as_secs() as i64);
    let now = now_unix();
    let skew = max_skew.as_secs() as i64;

    if this_update > now + skew {
        return Err(ocsp_error(format!(
            "OCSP 响应的 thisUpdate 在未来（{this_update} > {now}），检查系统时钟"
        )));
    }
    if let Some(next_update) = next_update
        && next_update + skew < now
    {
        return Err(ocsp_error(format!(
            "OCSP 响应已过期（nextUpdate = {next_update}，now = {now}）"
        )));
    }

    // 4) 验签
    verify_signature(&basic, issuer)?;

    Ok(Stapled {
        der: body.to_vec(),
        this_update,
        next_update,
    })
}

/// 校验 `BasicOCSPResponse` 的签名。
///
/// 签名者可能是 issuer 本身，也可能是 issuer 签发的**委派 responder**（`certs` 字段）。
/// `certs` 里可能有多张证书，逐张判定「是否获授权 + 能否验签」；都不成立时再退回 issuer。
fn verify_signature(basic: &BasicOcspResponse, issuer: &Certificate) -> AppResult<()> {
    let tbs = basic
        .tbs_response_data
        .to_der()
        .map_err(|error| ocsp_error(format!("编码 tbsResponseData 失败：{error}")))?;
    let signature = basic
        .signature
        .as_bytes()
        .ok_or_else(|| ocsp_error("OCSP 签名字段不是整字节的 BIT STRING"))?;

    let mut unauthorized = None;
    if let Some(certs) = &basic.certs {
        for candidate in certs {
            // 先查授权（便宜）：未获授权的证书连签名都不用验
            if let Err(error) = verify_responder_authorization(candidate, issuer) {
                unauthorized = Some(error);
                continue;
            }
            let spki = &candidate.tbs_certificate.subject_public_key_info;
            if verify_with_spki(
                &spki.algorithm.oid,
                spki.subject_public_key.raw_bytes(),
                &tbs,
                signature,
            ) {
                return Ok(());
            }
        }
    }

    // issuer 直签（最常见，也是唯一不依赖 `certs` 的路径）
    let spki = &issuer.tbs_certificate.subject_public_key_info;
    if verify_with_spki(
        &spki.algorithm.oid,
        spki.subject_public_key.raw_bytes(),
        &tbs,
        signature,
    ) {
        return Ok(());
    }

    Err(match unauthorized {
        Some(error) => ocsp_error(format!("OCSP 响应的签名无效：{error}")),
        None => ocsp_error("OCSP 响应的签名无效（issuer 与委派 responder 均不匹配）"),
    })
}

/// 判定一张证书是否有资格作为**委派 responder**（RFC 6960 §4.2.2.2）。
///
/// 只验「由同一 issuer 签发」是不够的：同一个中间 CA 给全世界签叶子证书，
/// 任何持有同 CA 普通证书的人都能伪造 `good` 响应。必须同时要求：
/// 1. 由本证书的 issuer 签发（名 + 签名）；
/// 2. 在有效期内；
/// 3. 不是 CA 证书；
/// 4. `extendedKeyUsage` 含 `id-kp-OCSPSigning`；
/// 5. `keyUsage`（若存在）含 `digitalSignature`。
fn verify_responder_authorization(responder: &Certificate, issuer: &Certificate) -> AppResult<()> {
    verify_certificate_signed_by(responder, issuer)?;
    verify_responder_usage(responder)
}

/// 校验委派 responder 证书的**用途**（EKU / KeyUsage / 非 CA）。
fn verify_responder_usage(responder: &Certificate) -> AppResult<()> {
    if let Some(extension) = find_extension(responder, BasicConstraints::OID) {
        let constraints =
            BasicConstraints::from_der(extension.extn_value.as_bytes()).map_err(|error| {
                ocsp_error(format!("解析委派证书的 basicConstraints 失败：{error}"))
            })?;
        if constraints.ca {
            return Err(ocsp_error(
                "委派 responder 证书是 CA 证书（basicConstraints.cA = true）",
            ));
        }
    }

    let extension = find_extension(responder, ExtendedKeyUsage::OID).ok_or_else(|| {
        ocsp_error("委派 responder 证书缺少 extendedKeyUsage（必须含 id-kp-OCSPSigning）")
    })?;
    let usage = ExtendedKeyUsage::from_der(extension.extn_value.as_bytes())
        .map_err(|error| ocsp_error(format!("解析委派证书的 extendedKeyUsage 失败：{error}")))?;
    if !usage.0.contains(&ID_KP_OCSP_SIGNING) {
        return Err(ocsp_error(
            "委派 responder 证书的 extendedKeyUsage 不含 id-kp-OCSPSigning\
             （同一 CA 签发的普通证书不能当 responder）",
        ));
    }

    if let Some(extension) = find_extension(responder, KeyUsage::OID) {
        let key_usage = KeyUsage::from_der(extension.extn_value.as_bytes())
            .map_err(|error| ocsp_error(format!("解析委派证书的 keyUsage 失败：{error}")))?;
        if !key_usage.digital_signature() {
            return Err(ocsp_error(
                "委派 responder 证书的 keyUsage 不含 digitalSignature",
            ));
        }
    }
    Ok(())
}

/// 取指定 OID 的扩展。
fn find_extension(
    cert: &Certificate,
    oid: const_oid::ObjectIdentifier,
) -> Option<&x509_cert::ext::Extension> {
    cert.tbs_certificate
        .extensions
        .as_ref()?
        .iter()
        .find(|extension| extension.extn_id == oid)
}

/// 校验 `child` 的签名确实由 `issuer` 签发，且 `child` 仍在有效期内。
fn verify_certificate_signed_by(child: &Certificate, issuer: &Certificate) -> AppResult<()> {
    if child.tbs_certificate.issuer != issuer.tbs_certificate.subject {
        return Err(ocsp_error(
            "OCSP 委派 responder 证书的 issuer 与签发者不匹配",
        ));
    }

    let tbs = child
        .tbs_certificate
        .to_der()
        .map_err(|error| ocsp_error(format!("编码委派证书 TBS 失败：{error}")))?;
    let signature = child
        .signature
        .as_bytes()
        .ok_or_else(|| ocsp_error("委派证书签名不是整字节的 BIT STRING"))?;

    let spki = &issuer.tbs_certificate.subject_public_key_info;
    if !verify_with_spki(
        &spki.algorithm.oid,
        spki.subject_public_key.raw_bytes(),
        &tbs,
        signature,
    ) {
        return Err(ocsp_error("OCSP 委派 responder 证书的签名无效"));
    }

    // 有效期（时间字段是 UTCTime / GeneralizedTime 的联合类型）
    let now = now_unix();
    let not_before = child
        .tbs_certificate
        .validity
        .not_before
        .to_unix_duration()
        .as_secs() as i64;
    let not_after = child
        .tbs_certificate
        .validity
        .not_after
        .to_unix_duration()
        .as_secs() as i64;
    if now < not_before || now > not_after {
        return Err(ocsp_error(format!(
            "OCSP 委派 responder 证书不在有效期内（{not_before}..{not_after}，now = {now}）"
        )));
    }
    Ok(())
}

/// 验证签名：遍历 rustls 支持的算法，按**公钥算法 OID** 精确匹配。
///
/// 用 `(OID, 公钥裸字节)` 而不是 `SubjectPublicKeyInfo`，避免为了一个类型引入 `spki` 依赖。
fn verify_with_spki(
    key_algorithm: &const_oid::ObjectIdentifier,
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> bool {
    let provider = CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let algorithms = provider.signature_verification_algorithms;

    let expected = key_algorithm.as_bytes();
    algorithms.all.iter().any(|algorithm| {
        // 只尝试公钥算法匹配的项：避免把 RSA 公钥喂给 ECDSA 校验器
        algorithm_oid(algorithm.public_key_alg_id().as_ref()) == Some(expected)
            && algorithm
                .verify_signature(public_key, message, signature)
                .is_ok()
    })
}

/// 从算法标识里取出 OID 内容字节。
///
/// 两种编码都要认：
/// - `SEQUENCE { OID, params }` —— 证书里 `SubjectPublicKeyInfo.algorithm` 的标准形式
/// - 裸 `OID [OID]` —— rustls 的 `SignatureVerificationAlgorithm::public_key_alg_id()` 形式
///   （ECDSA 是 `id-ecPublicKey` + 曲线 OID 两个 OID 连写，RSA 是 `rsaEncryption` + NULL）
fn algorithm_oid(der: &[u8]) -> Option<&[u8]> {
    let (tag, value, _) = read_tlv(der)?;
    match tag {
        0x06 => Some(value),
        0x30 => {
            let (inner_tag, oid, _) = read_tlv(value)?;
            (inner_tag == 0x06).then_some(oid)
        }
        _ => None,
    }
}

/// 读一个 DER TLV：返回 `(tag, value, rest)`。只支持单字节 tag 与定长长度。
fn read_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        // 长形式长度最多 4 字节（>4GB 的证书/响应不现实）
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (length_bytes, rest) = rest.split_at(count);
        let mut length = 0usize;
        for &byte in length_bytes {
            length = (length << 8) | usize::from(byte);
        }
        (length, rest)
    };
    if rest.len() < length {
        return None;
    }
    let (value, rest) = rest.split_at(length);
    Some((tag, value, rest))
}

/// 读响应体，硬上限 `max` 字节。
///
/// responder 被控（或 `[tls.ocsp].responder` 配错）时，`bytes()` 会把整个响应读进内存，
/// 一个几百 MB 的响应就能把进程打掉。正常 OCSP 响应 < 8 KiB。
async fn read_body(response: reqwest::Response, max: usize) -> AppResult<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > max as u64
    {
        return Err(ocsp_error(format!(
            "OCSP 响应过大（Content-Length = {length} > {max} 字节上限）"
        )));
    }

    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ocsp_error(format!("读取 OCSP 响应失败：{error}")))?
    {
        if body.len() + chunk.len() > max {
            return Err(ocsp_error(format!("OCSP 响应超过 {max} 字节上限，已丢弃")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 明文 responder 的告警。
///
/// 不直接拒绝：公共 CA 的 AIA 基本都是 `http://`，拒绝等于关掉 OCSP。
/// 但要留痕——链路上的中间人可以丢弃请求（软失败）、重放仍在有效期内的旧响应，
/// 从而压制吊销状态；签名只能保证**完整性**，保证不了**新鲜度**。
fn warn_insecure(url: String) -> String {
    if url.starts_with("http://") {
        tracing::warn!(
            %url,
            "OCSP responder 走明文 http://：链路上的中间人可压制/重放吊销状态，\
             只建议在可信网络内使用"
        );
    }
    url
}

/// 把 OCSP 响应挂到证书上（`CertifiedKey` 的 `ocsp` 字段）。
fn attach(entry: &CertEntry, stapled: &Stapled) -> CertifiedKey {
    let mut key = (*entry.key).clone();
    key.ocsp = Some(stapled.der.clone());
    key
}

/// 日志里的域名标签（默认证书的 `pattern` 是空串）。
fn domain_label(pattern: &str) -> &str {
    if pattern.is_empty() {
        "<default>"
    } else {
        pattern
    }
}

fn ocsp_error(message: impl Into<String>) -> AppError {
    AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_short_and_long_form_length() {
        // 0x30 0x03 0x02 0x01 0x00
        let (tag, value, rest) = read_tlv(&[0x30, 0x03, 0x02, 0x01, 0x00]).unwrap();
        assert_eq!(tag, 0x30);
        assert_eq!(value, &[0x02, 0x01, 0x00]);
        assert!(rest.is_empty());

        // 长形式：0x30 0x81 0x80 + 128 字节
        let mut input = vec![0x30, 0x81, 0x80];
        input.extend(std::iter::repeat_n(0xAA_u8, 128));
        let (tag, value, rest) = read_tlv(&input).unwrap();
        assert_eq!(tag, 0x30);
        assert_eq!(value.len(), 128);
        assert!(rest.is_empty());
    }

    #[test]
    fn should_reject_truncated_tlv() {
        assert!(read_tlv(&[0x30]).is_none());
        assert!(read_tlv(&[0x30, 0x05, 0x01]).is_none());
        assert!(read_tlv(&[0x30, 0x82, 0x01]).is_none());
    }

    #[test]
    fn should_extract_oid_from_algorithm_identifier() {
        // 形式 1：SEQUENCE { rsaEncryption, NULL }
        let wrapped = [
            0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05,
            0x00,
        ];
        let expected = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
        assert_eq!(algorithm_oid(&wrapped), Some(expected.as_slice()));

        // 形式 2：rustls 的裸 OID（rsaEncryption + NULL 连写）
        let bare = [
            0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
        ];
        assert_eq!(algorithm_oid(&bare), Some(expected.as_slice()));
    }

    #[test]
    fn should_match_ecdsa_public_key_oid() {
        // id-ecPublicKey 1.2.840.10045.2.1 + secp256r1（rustls 的 ECDSA 条目就长这样）
        let der = [
            0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48,
            0xce, 0x3d, 0x03, 0x01, 0x07,
        ];
        let oid = algorithm_oid(&der).expect("应取到 id-ecPublicKey");
        assert_eq!(oid, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]);
    }

    #[test]
    fn should_label_default_certificate() {
        assert_eq!(domain_label(""), "<default>");
        assert_eq!(domain_label("api.example.com"), "api.example.com");
    }

    #[test]
    fn should_reject_garbage_response() {
        let cert = Certificate::from_der(&[]);
        assert!(cert.is_err(), "空输入应解析失败");
    }
}
