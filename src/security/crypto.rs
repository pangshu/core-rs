//! 加解密与签名。
//!
//! - AES-256-GCM / ChaCha20-Poly1305：AEAD，密文自带版本头
//!   `version(1) || algorithm(1) || nonce(12) || ciphertext||tag`
//! - HMAC-SHA256：消息认证码
//! - RSA：PKCS#1 v1.5（兼容既有系统）与 PSS（新系统推荐）签名/验签
//! - 常量时间比较：防时序侧信道
//!
//! # 密钥处理（fail-closed）
//!
//! 密钥长度不足一律**报错**，绝不补齐或截断——把「配置写错」变成「用一个
//! 可预测的弱密钥加密」，是比不加密更危险的 fail-open。密钥经 [`Secret`] 持有，
//! `Drop` 时清零。
//!
//! 由任意长度材料（如 64 位 hex 字符串）构造时走 HKDF-SHA256 派生
//! （[`Cipher::from_key_material`]），而不是取前 32 字节——直接截断会让
//! 256 bit 熵的密钥退化成 128 bit。

use std::fmt;

use aes_gcm::aead::{Aead, AeadCore, OsRng, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use chacha20poly1305::{ChaCha20Poly1305, Key as ChaChaKey, Nonce as ChaChaNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rsa::pkcs1::{DecodeRsaPublicKey, EncodeRsaPublicKey};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::pss;
use rsa::signature::{RandomizedSigner, SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::config::sections::security::{CryptoAlgorithm, SecuritySection};
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::security::Secret;

/// 对称密钥长度（字节）。
pub const KEY_LEN: usize = 32;

/// AEAD nonce 长度（字节）。
pub const NONCE_LEN: usize = 12;

/// AEAD 认证标签长度（字节）。
pub const TAG_LEN: usize = 16;

/// 密文头长度：`version(1) || algorithm(1) || nonce(12)`。
pub const HEADER_LEN: usize = 2 + NONCE_LEN;

/// 最短合法密文长度：头 + 标签（空明文）。
pub const MIN_BLOB_LEN: usize = HEADER_LEN + TAG_LEN;

/// 密文格式版本。**升级格式必须同时升级它**，否则历史数据无法区分。
pub const BLOB_VERSION: u8 = 1;

/// RSA 密钥位数下限。低于 2048 bit 的密钥不应再用于新签名。
pub const MIN_RSA_BITS: usize = 2048;

/// RSA 密钥位数上限（防配置写成天文数字后启动期卡死）。
pub const MAX_RSA_BITS: usize = 8192;

/// 密文头里的算法标识：AES-256-GCM。
const ALG_AES_GCM: u8 = 1;

/// 密文头里的算法标识：ChaCha20-Poly1305。
const ALG_CHACHA20_POLY1305: u8 = 2;

/// HKDF 的 `info`：把「加密密钥派生」与其他用途（如 HMAC）域分离。
const HKDF_INFO: &[u8] = b"aurora/security/crypto/v1";

/// 严格取 32 字节密钥：长度不符直接报错（**不补齐、不截断**）。
fn exact_key(key: &[u8]) -> AppResult<[u8; KEY_LEN]> {
    if key.len() != KEY_LEN {
        return Err(AppError::invalid_param(format!(
            "对称密钥必须为 {KEY_LEN} 字节，当前 {} 字节",
            key.len()
        )));
    }
    let mut buf = [0u8; KEY_LEN];
    buf.copy_from_slice(key);
    Ok(buf)
}

/// 由任意长度材料经 HKDF-SHA256 派生 32 字节密钥。
///
/// 材料不足 32 字节时拒绝：短口令派生出的「合法 32 字节密钥」只是把弱密钥
/// 藏得更深（熵仍是口令的熵）。
fn derived_key(material: &[u8]) -> AppResult<[u8; KEY_LEN]> {
    if material.len() < KEY_LEN {
        return Err(AppError::invalid_param(format!(
            "密钥材料至少需要 {KEY_LEN} 字节，当前 {} 字节",
            material.len()
        )));
    }
    let hkdf = Hkdf::<Sha256>::new(None, material);
    let mut out = [0u8; KEY_LEN];
    hkdf.expand(HKDF_INFO, &mut out)
        .map_err(|e| AppError::internal(anyhow::anyhow!("hkdf expand failed: {e}")))?;
    Ok(out)
}

/// 算法 → 密文头里的标识字节。
const fn algorithm_tag(algorithm: CryptoAlgorithm) -> u8 {
    match algorithm {
        CryptoAlgorithm::AesGcm => ALG_AES_GCM,
        CryptoAlgorithm::Chacha20poly1305 => ALG_CHACHA20_POLY1305,
    }
}

/// 密文头里的标识字节 → 算法。
fn algorithm_from_tag(tag: u8) -> AppResult<CryptoAlgorithm> {
    match tag {
        ALG_AES_GCM => Ok(CryptoAlgorithm::AesGcm),
        ALG_CHACHA20_POLY1305 => Ok(CryptoAlgorithm::Chacha20poly1305),
        _ => Err(decrypt_failed()),
    }
}

/// 「密文无效」的对外错误（4xx）。
///
/// 与 [`AppError::internal`]（`50001`）分开：密文可能由攻击者提供，认证失败
/// 是**预期内的输入错误**，不该表现成服务端 500（既误导排障，也给了
/// 「刷 500 触发告警」的手段）。
fn decrypt_failed() -> AppError {
    AppError::biz(ErrorCode::DECRYPT_FAILED, "密文无效或已被篡改")
}

/// 切出 `(algorithm, nonce, ciphertext||tag)`。
fn split_blob(blob: &[u8]) -> AppResult<(CryptoAlgorithm, &[u8], &[u8])> {
    if blob.len() < MIN_BLOB_LEN {
        return Err(decrypt_failed());
    }
    if blob[0] != BLOB_VERSION {
        // 未知版本按「无效密文」处理（fail-closed）：不猜格式、不降级。
        return Err(decrypt_failed());
    }
    let algorithm = algorithm_from_tag(blob[1])?;
    let nonce = &blob[2..HEADER_LEN];
    let ciphertext = &blob[HEADER_LEN..];
    Ok((algorithm, nonce, ciphertext))
}

/// AEAD 加密并拼上版本头。
fn seal(
    algorithm: CryptoAlgorithm,
    key: &[u8; KEY_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> AppResult<Vec<u8>> {
    // 两种算法的 nonce 都是 96 bit，随机生成；同一密钥下 2^32 次加密后应轮换。
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let payload = Payload {
        msg: plaintext,
        aad,
    };
    let ciphertext = match algorithm {
        CryptoAlgorithm::AesGcm => {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|e| AppError::internal(anyhow::anyhow!("invalid aes-gcm key: {e}")))?;
            cipher
                .encrypt(&nonce, payload)
                .map_err(|e| AppError::internal(anyhow::anyhow!("aes-gcm encrypt failed: {e}")))?
        }
        CryptoAlgorithm::Chacha20poly1305 => {
            let cipher = ChaCha20Poly1305::new(ChaChaKey::from_slice(key));
            cipher
                .encrypt(ChaChaNonce::from_slice(nonce.as_slice()), payload)
                .map_err(|e| AppError::internal(anyhow::anyhow!("chacha20 encrypt failed: {e}")))?
        }
    };

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.push(BLOB_VERSION);
    out.push(algorithm_tag(algorithm));
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// AEAD 解密（已切好 nonce 与密文）。认证失败统一映射为 [`ErrorCode::DECRYPT_FAILED`]。
fn open(
    algorithm: CryptoAlgorithm,
    key: &[u8; KEY_LEN],
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> AppResult<Vec<u8>> {
    let payload = Payload {
        msg: ciphertext,
        aad,
    };
    match algorithm {
        CryptoAlgorithm::AesGcm => {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|e| AppError::internal(anyhow::anyhow!("invalid aes-gcm key: {e}")))?;
            cipher
                .decrypt(Nonce::from_slice(nonce), payload)
                .map_err(|_| decrypt_failed())
        }
        CryptoAlgorithm::Chacha20poly1305 => {
            let cipher = ChaCha20Poly1305::new(ChaChaKey::from_slice(key));
            cipher
                .decrypt(ChaChaNonce::from_slice(nonce), payload)
                .map_err(|_| decrypt_failed())
        }
    }
}

/// 解出密文头声明的算法，并要求与 `expected` 一致。
fn expect_algorithm(blob: &[u8], expected: CryptoAlgorithm) -> AppResult<(&[u8], &[u8])> {
    let (algorithm, nonce, ciphertext) = split_blob(blob)?;
    if algorithm != expected {
        return Err(decrypt_failed());
    }
    Ok((nonce, ciphertext))
}

/// AES-256-GCM 加解密。
#[derive(Clone)]
pub struct AesGcmCipher {
    key: Secret<[u8; KEY_LEN]>,
}

impl fmt::Debug for AesGcmCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AesGcmCipher").finish_non_exhaustive()
    }
}

impl AesGcmCipher {
    /// 由 32 字节密钥构造。
    #[must_use]
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self {
            key: Secret::new(*key),
        }
    }

    /// 由字节切片构造：**长度必须恰好 32 字节**，否则报错。
    ///
    /// # Errors
    /// 长度不符时返回 `10001`。
    pub fn from_slice(key: &[u8]) -> AppResult<Self> {
        Ok(Self {
            key: Secret::new(exact_key(key)?),
        })
    }

    /// 由任意长度（≥32 字节）密钥材料经 HKDF-SHA256 派生。
    ///
    /// # Errors
    /// 材料不足 32 字节时返回 `10001`；HKDF 失败时返回 `50001`。
    pub fn from_key_material(material: &[u8]) -> AppResult<Self> {
        Ok(Self {
            key: Secret::new(derived_key(material)?),
        })
    }

    /// 加密：输出 `version || algorithm || nonce || ciphertext || tag`。
    ///
    /// # Errors
    /// 底层 AEAD 失败时返回内部错误（`50001`）。
    pub fn encrypt(&self, plaintext: &[u8]) -> AppResult<Vec<u8>> {
        self.encrypt_with_aad(plaintext, &[])
    }

    /// 带附加认证数据（AAD）的加密。
    ///
    /// AAD 不加密但参与认证：把密文绑定到上下文（如 `users:ssn:{id}`）后，
    /// 拿到 DB 写权限的人**无法把密文从一行搬到另一行**——AAD 不匹配即认证失败。
    ///
    /// # Errors
    /// 见 [`Self::encrypt`]。
    pub fn encrypt_with_aad(&self, plaintext: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        seal(CryptoAlgorithm::AesGcm, self.key.expose(), plaintext, aad)
    }

    /// 解密 [`Self::encrypt`] 的输出。
    ///
    /// # Errors
    /// 数据被篡改 / 过短 / 认证失败 / 算法标识不符时返回 `10008`（400）。
    pub fn decrypt(&self, blob: &[u8]) -> AppResult<Vec<u8>> {
        self.decrypt_with_aad(blob, &[])
    }

    /// 带 AAD 的解密（AAD 必须与加密时**逐字节一致**）。
    ///
    /// # Errors
    /// 见 [`Self::decrypt`]。
    pub fn decrypt_with_aad(&self, blob: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        let (nonce, ciphertext) = expect_algorithm(blob, CryptoAlgorithm::AesGcm)?;
        open(
            CryptoAlgorithm::AesGcm,
            self.key.expose(),
            nonce,
            ciphertext,
            aad,
        )
    }
}

/// ChaCha20-Poly1305 加解密（API 与 [`AesGcmCipher`] 相同）。
#[derive(Clone)]
pub struct ChaChaCipher {
    key: Secret<[u8; KEY_LEN]>,
}

impl fmt::Debug for ChaChaCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChaChaCipher").finish_non_exhaustive()
    }
}

impl ChaChaCipher {
    /// 由 32 字节密钥构造。
    #[must_use]
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self {
            key: Secret::new(*key),
        }
    }

    /// 由字节切片构造：**长度必须恰好 32 字节**，否则报错。
    ///
    /// # Errors
    /// 长度不符时返回 `10001`。
    pub fn from_slice(key: &[u8]) -> AppResult<Self> {
        Ok(Self {
            key: Secret::new(exact_key(key)?),
        })
    }

    /// 由任意长度（≥32 字节）密钥材料经 HKDF-SHA256 派生。
    ///
    /// # Errors
    /// 材料不足 32 字节时返回 `10001`；HKDF 失败时返回 `50001`。
    pub fn from_key_material(material: &[u8]) -> AppResult<Self> {
        Ok(Self {
            key: Secret::new(derived_key(material)?),
        })
    }

    /// 加密：输出 `version || algorithm || nonce || ciphertext || tag`。
    ///
    /// # Errors
    /// 底层 AEAD 失败时返回内部错误（`50001`）。
    pub fn encrypt(&self, plaintext: &[u8]) -> AppResult<Vec<u8>> {
        self.encrypt_with_aad(plaintext, &[])
    }

    /// 带 AAD 的加密（见 [`AesGcmCipher::encrypt_with_aad`]）。
    ///
    /// # Errors
    /// 见 [`Self::encrypt`]。
    pub fn encrypt_with_aad(&self, plaintext: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        seal(
            CryptoAlgorithm::Chacha20poly1305,
            self.key.expose(),
            plaintext,
            aad,
        )
    }

    /// 解密 [`Self::encrypt`] 的输出。
    ///
    /// # Errors
    /// 数据被篡改 / 过短 / 认证失败 / 算法标识不符时返回 `10008`（400）。
    pub fn decrypt(&self, blob: &[u8]) -> AppResult<Vec<u8>> {
        self.decrypt_with_aad(blob, &[])
    }

    /// 带 AAD 的解密。
    ///
    /// # Errors
    /// 见 [`Self::decrypt`]。
    pub fn decrypt_with_aad(&self, blob: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        let (nonce, ciphertext) = expect_algorithm(blob, CryptoAlgorithm::Chacha20poly1305)?;
        open(
            CryptoAlgorithm::Chacha20poly1305,
            self.key.expose(),
            nonce,
            ciphertext,
            aad,
        )
    }
}

/// 由 `[security].crypto` 装配的加密器。
///
/// 相比直接用 [`AesGcmCipher`] / [`ChaChaCipher`]，它多了三件事：
///
/// 1. **算法由配置决定**（`crypto.algorithm`），密文头里记录实际算法，
///    解密时按头分发——换算法不需要迁移历史数据；
/// 2. **密钥轮换**：`crypto.previous_key` 只用于解密，过渡期内新旧密文都能解；
/// 3. **未配置即拒绝**：[`Self::from_config`] 在 `crypto.key` 为空时返回 `None`，
///    取用时经 [`SecurityContext`](crate::security::SecurityContext) 报错，
///    不存在「空密钥 = 全零密钥」的降级路径。
#[derive(Clone)]
pub struct Cipher {
    algorithm: CryptoAlgorithm,
    current: Secret<[u8; KEY_LEN]>,
    previous: Option<Secret<[u8; KEY_LEN]>>,
}

impl fmt::Debug for Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cipher")
            .field("algorithm", &self.algorithm)
            .field("has_previous_key", &self.previous.is_some())
            .finish_non_exhaustive()
    }
}

impl Cipher {
    /// 由 32 字节密钥与算法构造。
    #[must_use]
    pub fn new(algorithm: CryptoAlgorithm, key: &[u8; KEY_LEN]) -> Self {
        Self {
            algorithm,
            current: Secret::new(*key),
            previous: None,
        }
    }

    /// 由字节切片构造（长度必须恰好 32 字节）。
    ///
    /// # Errors
    /// 长度不符时返回 `10001`。
    pub fn from_slice(algorithm: CryptoAlgorithm, key: &[u8]) -> AppResult<Self> {
        Ok(Self::new(algorithm, &exact_key(key)?))
    }

    /// 由任意长度（≥32 字节）密钥材料经 HKDF-SHA256 派生。
    ///
    /// # Errors
    /// 材料不足 32 字节时返回 `10001`；HKDF 失败时返回 `50001`。
    pub fn from_key_material(algorithm: CryptoAlgorithm, material: &[u8]) -> AppResult<Self> {
        Ok(Self::new(algorithm, &derived_key(material)?))
    }

    /// 设置「上一代密钥」：仅用于解密，支持轮换过渡期。
    ///
    /// # Errors
    /// 长度不符时返回 `10001`。
    pub fn with_previous_key(mut self, key: &[u8]) -> AppResult<Self> {
        self.previous = Some(Secret::new(exact_key(key)?));
        Ok(self)
    }

    /// 由配置装配。
    ///
    /// - `crypto.key` 为空 → `Ok(None)`（**显式关闭**对称加密，不降级成全零密钥）；
    /// - `crypto.previous_key` 非空但 `crypto.key` 为空 → 报错（过渡期配置不完整）。
    ///
    /// # Errors
    /// 密钥长度不足、`previous_key` 单独出现时返回 `10001`。
    pub fn from_config(section: &SecuritySection) -> AppResult<Option<Self>> {
        let crypto = &section.crypto;
        let current = crypto.key.trim();
        let previous = crypto.previous_key.trim();

        if current.is_empty() {
            if !previous.is_empty() {
                return Err(AppError::invalid_param(
                    "[security].crypto.previous_key 已配置但 [security].crypto.key 为空；\
                     轮换过渡期必须同时提供新密钥",
                ));
            }
            return Ok(None);
        }

        let mut cipher = Self::from_key_material(crypto.algorithm, current.as_bytes())?;
        if !previous.is_empty() {
            cipher = cipher.with_previous_key(&derived_key(previous.as_bytes())?)?;
        }
        Ok(Some(cipher))
    }

    /// 当前算法。
    #[must_use]
    pub fn algorithm(&self) -> CryptoAlgorithm {
        self.algorithm
    }

    /// 是否处于轮换过渡期（配置了上一代密钥）。
    #[must_use]
    pub fn has_previous_key(&self) -> bool {
        self.previous.is_some()
    }

    /// 用当前算法加密（无 AAD）。
    ///
    /// # Errors
    /// 底层 AEAD 失败时返回内部错误（`50001`）。
    pub fn encrypt(&self, plaintext: &[u8]) -> AppResult<Vec<u8>> {
        self.encrypt_with_aad(plaintext, &[])
    }

    /// 用当前算法加密，并绑定 AAD。
    ///
    /// # Errors
    /// 见 [`Self::encrypt`]。
    pub fn encrypt_with_aad(&self, plaintext: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        seal(self.algorithm, self.current.expose(), plaintext, aad)
    }

    /// 解密（无 AAD）。
    ///
    /// # Errors
    /// 密文无效 / 认证失败 / 所有密钥都不匹配时返回 `10008`（400）。
    pub fn decrypt(&self, blob: &[u8]) -> AppResult<Vec<u8>> {
        self.decrypt_with_aad(blob, &[])
    }

    /// 解密：先试当前密钥，再试上一代密钥（轮换过渡期）。
    ///
    /// # Errors
    /// 见 [`Self::decrypt`]。
    pub fn decrypt_with_aad(&self, blob: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        let (algorithm, nonce, ciphertext) = split_blob(blob)?;

        if let Ok(plaintext) = open(algorithm, self.current.expose(), nonce, ciphertext, aad) {
            return Ok(plaintext);
        }
        if let Some(previous) = &self.previous
            && let Ok(plaintext) = open(algorithm, previous.expose(), nonce, ciphertext, aad)
        {
            tracing::debug!(?algorithm, "密文由上一代密钥解出（轮换过渡期）");
            return Ok(plaintext);
        }

        // 认证失败既可能是「被篡改」也可能是「密钥配错」——两者对外都是 4xx，
        // 但对内必须留痕，否则全站解密失败时无从下手。
        tracing::warn!(
            ?algorithm,
            blob_len = blob.len(),
            has_previous_key = self.previous.is_some(),
            "解密失败：所有可用密钥均无法认证该密文（可能被篡改，或密钥不匹配）"
        );
        Err(decrypt_failed())
    }
}

/// HMAC-SHA256 消息认证码。
#[derive(Clone)]
pub struct HmacSigner {
    key: Secret<Vec<u8>>,
}

impl fmt::Debug for HmacSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HmacSigner").finish_non_exhaustive()
    }
}

impl HmacSigner {
    /// 由密钥构造：**空密钥与短于 32 字节的密钥一律拒绝**。
    ///
    /// HMAC 接受任意长度密钥（包括 0 字节），空密钥的 MAC 任何人都能重算——
    /// 静默接受它等于把签名变成校验和。
    ///
    /// # Errors
    /// 密钥短于 32 字节时返回 `10001`。
    pub fn try_new(key: impl AsRef<[u8]>) -> AppResult<Self> {
        let key = key.as_ref();
        if key.len() < KEY_LEN {
            return Err(AppError::invalid_param(format!(
                "HMAC 密钥至少需要 {KEY_LEN} 字节，当前 {} 字节",
                key.len()
            )));
        }
        Ok(Self {
            key: Secret::new(key.to_vec()),
        })
    }

    /// 由配置装配：`[security].hmac.key` 为空 → `Ok(None)`。
    ///
    /// # Errors
    /// 密钥非空但不足 32 字节时返回 `10001`。
    pub fn from_config(section: &SecuritySection) -> AppResult<Option<Self>> {
        let key = section.hmac.key.trim();
        if key.is_empty() {
            return Ok(None);
        }
        Self::try_new(key.as_bytes()).map(Some)
    }

    /// 计算 `data` 的 HMAC-SHA256，返回 32 字节摘要。
    #[must_use]
    pub fn digest(&self, data: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.key.expose().as_slice())
            .expect("HMAC 接受任意长度密钥");
        mac.update(data);
        let bytes = mac.finalize().into_bytes();
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }

    /// 计算 base64（URL-safe，无填充）签名。
    #[must_use]
    pub fn sign_b64(&self, data: &[u8]) -> String {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        URL_SAFE_NO_PAD.encode(self.digest(data))
    }

    /// 以常量时间比较校验 base64 签名。
    ///
    /// **不匹配返回 `Err`（`20001`，401）而不是 `Ok(false)`**：返回 `bool` 时
    /// `signer.verify_b64(..)?;`（漏写 `if`）编译无警告、鉴权静默放行。
    /// 需要布尔语义请用 [`Self::is_valid_b64`]。
    ///
    /// # Errors
    /// 签名编码非法或与数据不匹配时返回 `20001`。
    pub fn verify_b64(&self, data: &[u8], signature_b64: &str) -> AppResult<()> {
        if self.is_valid_b64(data, signature_b64) {
            Ok(())
        } else {
            Err(AppError::unauthenticated("签名校验失败"))
        }
    }

    /// 校验 base64 签名，返回布尔值（不匹配即 `false`，编码非法也是 `false`）。
    #[must_use]
    pub fn is_valid_b64(&self, data: &[u8], signature_b64: &str) -> bool {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let Ok(expected) = URL_SAFE_NO_PAD.decode(signature_b64.as_bytes()) else {
            return false;
        };
        bool::from(self.digest(data).ct_eq(expected.as_slice()))
    }
}

/// RSA 签名器（PKCS#1 v1.5 或 PSS，SHA-256）。
///
/// 跨服务验签请把 [`Self::public_key_pem`] 交给对端，对端用
/// [`RsaVerifier::from_public_key_pem`] 构造**只验签**的实例。
#[derive(Clone)]
pub struct RsaSigner {
    private_key: RsaPrivateKey,
}

impl fmt::Debug for RsaSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RsaSigner").finish_non_exhaustive()
    }
}

impl RsaSigner {
    /// 生成指定位数的 RSA 密钥对（2048..=8192）。
    ///
    /// # Errors
    /// 位数越界返回 `10001`；密钥生成失败返回 `50001`。
    pub fn generate(bits: usize) -> AppResult<Self> {
        if !(MIN_RSA_BITS..=MAX_RSA_BITS).contains(&bits) {
            return Err(AppError::invalid_param(format!(
                "RSA 密钥位数必须在 {MIN_RSA_BITS}..={MAX_RSA_BITS} 之间，当前 {bits}"
            )));
        }
        let mut rng = OsRng;
        let private_key = RsaPrivateKey::new(&mut rng, bits)
            .map_err(|e| AppError::internal(anyhow::anyhow!("rsa keygen failed: {e}")))?;
        Ok(Self { private_key })
    }

    /// 对应的公钥（PEM，PKCS#1）。
    ///
    /// # Errors
    /// PEM 编码失败返回 `50001`（**不再**用 `unwrap_or_default` 返回空串——
    /// 空公钥发给对端等于关掉验签）。
    pub fn public_key_pem(&self) -> AppResult<String> {
        self.private_key
            .to_public_key()
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| AppError::internal(anyhow::anyhow!("encode public key failed: {e}")))
    }

    /// 只验签的实例（同一密钥对）。
    #[must_use]
    pub fn verifier(&self) -> RsaVerifier {
        RsaVerifier::new(self.private_key.to_public_key())
    }

    /// 签名（PKCS#1 v1.5 + SHA-256，确定性）。
    ///
    /// # Errors
    /// 签名失败时返回内部错误（`50001`）。
    pub fn sign(&self, data: &[u8]) -> AppResult<Vec<u8>> {
        let signing_key = SigningKey::<Sha256>::new(self.private_key.clone());
        let signature: Signature = Signer::sign(&signing_key, data);
        Ok(signature.to_vec())
    }

    /// 签名（PSS + SHA-256，随机化）。新系统优先用它。
    ///
    /// # Errors
    /// 签名失败时返回内部错误（`50001`）。
    pub fn sign_pss(&self, data: &[u8]) -> AppResult<Vec<u8>> {
        let signing_key = pss::SigningKey::<Sha256>::new(self.private_key.clone());
        let signature = signing_key
            .try_sign_with_rng(&mut OsRng, data)
            .map_err(|e| AppError::internal(anyhow::anyhow!("rsa pss sign failed: {e}")))?;
        Ok(signature.to_vec())
    }

    /// 用自身公钥验签（PKCS#1 v1.5）。跨服务场景请用 [`Self::verifier`]。
    ///
    /// # Errors
    /// 仅内部错误；**签名格式非法返回 `Ok(false)`**（不把攻击者可控的畸形
    /// 输入变成 500）。
    pub fn verify(&self, data: &[u8], signature: &[u8]) -> AppResult<bool> {
        self.verifier().verify(data, signature)
    }
}

/// RSA 公钥验签器（只有公钥，无法签名）。
#[derive(Clone)]
pub struct RsaVerifier {
    public_key: RsaPublicKey,
}

impl fmt::Debug for RsaVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RsaVerifier").finish_non_exhaustive()
    }
}

impl RsaVerifier {
    /// 由公钥构造。
    #[must_use]
    pub fn new(public_key: RsaPublicKey) -> Self {
        Self { public_key }
    }

    /// 由 PEM 公钥构造（同时接受 PKCS#1 `BEGIN RSA PUBLIC KEY` 与
    /// SPKI `BEGIN PUBLIC KEY`）。
    ///
    /// # Errors
    /// PEM 无法解析时返回 `10001`（细节只进日志，不回显给对端）。
    pub fn from_public_key_pem(pem: &str) -> AppResult<Self> {
        let parsed =
            RsaPublicKey::from_pkcs1_pem(pem).or_else(|_| RsaPublicKey::from_public_key_pem(pem));
        match parsed {
            Ok(public_key) => Ok(Self { public_key }),
            Err(error) => {
                tracing::warn!(error = %error, "RSA 公钥 PEM 解析失败");
                Err(AppError::invalid_param("RSA 公钥格式非法"))
            }
        }
    }

    /// 验签（PKCS#1 v1.5 + SHA-256）。
    ///
    /// 签名格式非法 / 长度不符 → `Ok(false)`；只有内部错误才 `Err`。
    ///
    /// # Errors
    /// 仅内部错误。
    pub fn verify(&self, data: &[u8], signature: &[u8]) -> AppResult<bool> {
        let Ok(signature) = Signature::try_from(signature) else {
            return Ok(false);
        };
        let verifying_key = VerifyingKey::<Sha256>::new(self.public_key.clone());
        Ok(verifying_key.verify(data, &signature).is_ok())
    }

    /// 验签（PSS + SHA-256）。
    ///
    /// # Errors
    /// 仅内部错误。
    pub fn verify_pss(&self, data: &[u8], signature: &[u8]) -> AppResult<bool> {
        let Ok(signature) = pss::Signature::try_from(signature) else {
            return Ok(false);
        };
        let verifying_key = pss::VerifyingKey::<Sha256>::new(self.public_key.clone());
        Ok(verifying_key.verify(data, &signature).is_ok())
    }
}

/// 常量时间比较（防时序侧信道）。
///
/// **只在长度相等时是常量时间的**：长度不等立即返回 `false`（长度本身会泄漏）。
/// 因此校验 MAC / token 时应先确认长度固定，或直接比较摘要值。
///
/// 实现与 `utils::constant_time_eq` 是**同一份**（CSRF / metrics / 限流也用它）——
/// 常量时间比较属于基础工具，不该随 `security` feature 开关出现两套实现。
pub use crate::utils::ct::constant_time_eq;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sections::security::{CryptoSecurity, HmacSecurity};

    #[test]
    fn should_roundtrip_aes_gcm() {
        let cipher = AesGcmCipher::new(&[7u8; 32]);
        let blob = cipher.encrypt(b"top secret").unwrap();
        assert_ne!(&blob[HEADER_LEN..], b"top secret", "密文不应等于明文");
        assert_eq!(cipher.decrypt(&blob).unwrap(), b"top secret");
    }

    #[test]
    fn should_roundtrip_chacha() {
        let cipher = ChaChaCipher::new(&[9u8; 32]);
        let blob = cipher.encrypt(b"hello").unwrap();
        assert_eq!(cipher.decrypt(&blob).unwrap(), b"hello");
    }

    #[test]
    fn should_fail_on_tampered_ciphertext() {
        let cipher = AesGcmCipher::new(&[7u8; 32]);
        let mut blob = cipher.encrypt(b"top secret").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        let error = cipher.decrypt(&blob).unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::DECRYPT_FAILED,
            "篡改应报密文无效（4xx），而不是 500"
        );
    }

    #[test]
    fn should_reject_short_or_foreign_blob() {
        let cipher = AesGcmCipher::new(&[7u8; 32]);
        assert_eq!(
            cipher.decrypt(b"short").unwrap_err().code(),
            ErrorCode::DECRYPT_FAILED
        );

        // 算法标识不符：ChaCha 的密文不能被 AES 实例解开
        let chacha = ChaChaCipher::new(&[7u8; 32]);
        let blob = chacha.encrypt(b"x").unwrap();
        assert_eq!(
            cipher.decrypt(&blob).unwrap_err().code(),
            ErrorCode::DECRYPT_FAILED
        );
    }

    #[test]
    fn should_reject_key_of_wrong_length() {
        // 空密钥 = 全零密钥是典型的 fail-open，必须报错
        assert!(AesGcmCipher::from_slice(&[]).is_err());
        assert!(AesGcmCipher::from_slice(&[1u8; 31]).is_err());
        assert!(AesGcmCipher::from_slice(&[1u8; 33]).is_err());
        assert!(AesGcmCipher::from_slice(&[1u8; 32]).is_ok());
    }

    #[test]
    fn should_derive_distinct_keys_from_distinct_material() {
        let a = AesGcmCipher::from_key_material(&[1u8; 64]).unwrap();
        let b = AesGcmCipher::from_key_material(&[2u8; 64]).unwrap();
        let blob = a.encrypt(b"payload").unwrap();
        assert!(b.decrypt(&blob).is_err(), "不同材料的派生密钥不能互解");

        // 同一材料派生结果稳定
        let a2 = AesGcmCipher::from_key_material(&[1u8; 64]).unwrap();
        assert_eq!(a2.decrypt(&blob).unwrap(), b"payload");
        // 短材料拒绝
        assert!(AesGcmCipher::from_key_material(&[1u8; 31]).is_err());
    }

    #[test]
    fn should_bind_ciphertext_to_aad() {
        let cipher = Cipher::new(CryptoAlgorithm::AesGcm, &[3u8; 32]);
        let blob = cipher
            .encrypt_with_aad(b"13800138000", b"users:phone:42")
            .unwrap();
        assert_eq!(
            cipher.decrypt_with_aad(&blob, b"users:phone:42").unwrap(),
            b"13800138000"
        );
        // 换一行（AAD 变了）必须解不开：否则密文可在行间搬运
        assert_eq!(
            cipher
                .decrypt_with_aad(&blob, b"users:phone:43")
                .unwrap_err()
                .code(),
            ErrorCode::DECRYPT_FAILED
        );
    }

    #[test]
    fn should_decrypt_with_previous_key_during_rotation() {
        let old = Cipher::new(CryptoAlgorithm::AesGcm, &[1u8; 32]);
        let blob = old.encrypt(b"legacy").unwrap();

        let rotated = Cipher::new(CryptoAlgorithm::AesGcm, &[2u8; 32])
            .with_previous_key(&[1u8; 32])
            .unwrap();
        assert!(rotated.has_previous_key());
        assert_eq!(rotated.decrypt(&blob).unwrap(), b"legacy");

        // 轮换后的新密文用新密钥，且旧实例解不开
        let fresh = rotated.encrypt(b"new").unwrap();
        assert_eq!(rotated.decrypt(&fresh).unwrap(), b"new");
        assert!(old.decrypt(&fresh).is_err());
    }

    #[test]
    fn should_switch_algorithm_without_migrating_history() {
        let aes = Cipher::new(CryptoAlgorithm::AesGcm, &[5u8; 32]);
        let blob = aes.encrypt(b"data").unwrap();

        // 同一密钥换成 ChaCha：新密文用 ChaCha，旧密文仍能按头解出
        let chacha = Cipher::new(CryptoAlgorithm::Chacha20poly1305, &[5u8; 32]);
        let new_blob = chacha.encrypt(b"data").unwrap();
        assert_eq!(new_blob[1], ALG_CHACHA20_POLY1305);
        assert_eq!(chacha.decrypt(&new_blob).unwrap(), b"data");
        assert_eq!(
            chacha.decrypt(&blob).unwrap(),
            b"data",
            "密文头记录了算法，换算法不应让历史数据失效"
        );
    }

    #[test]
    fn should_return_none_when_crypto_key_is_empty() {
        let section = SecuritySection::default();
        assert!(Cipher::from_config(&section).unwrap().is_none());

        // 只配了上一代密钥 = 过渡期配置不完整
        let section = SecuritySection {
            crypto: CryptoSecurity {
                previous_key: "0123456789abcdef0123456789abcdef".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        assert!(Cipher::from_config(&section).is_err());
    }

    #[test]
    fn should_build_cipher_from_config() {
        let section = SecuritySection {
            crypto: CryptoSecurity {
                key: "0123456789abcdef0123456789abcdef".to_string(),
                previous_key: "fedcba9876543210fedcba9876543210".to_string(),
                ..CryptoSecurity::default()
            },
            ..SecuritySection::default()
        };
        let cipher = Cipher::from_config(&section).unwrap().unwrap();
        assert_eq!(cipher.algorithm(), CryptoAlgorithm::AesGcm);
        assert!(cipher.has_previous_key());
    }

    #[test]
    fn should_roundtrip_hmac() {
        let signer = HmacSigner::try_new("0123456789abcdef0123456789abcdef").unwrap();
        let sig = signer.sign_b64(b"payload");
        signer.verify_b64(b"payload", &sig).unwrap();
        assert!(!signer.is_valid_b64(b"payload2", &sig));
        assert!(signer.verify_b64(b"payload2", &sig).is_err());
        assert!(signer.verify_b64(b"payload", "!!!not-base64!!!").is_err());
    }

    #[test]
    fn should_reject_empty_or_short_hmac_key() {
        // 空密钥的 HMAC 任何人都能重算
        assert!(HmacSigner::try_new("").is_err());
        assert!(HmacSigner::try_new("secret").is_err());
        assert!(HmacSigner::try_new([0u8; 32]).is_ok());
    }

    #[test]
    fn should_roundtrip_rsa() {
        let signer = RsaSigner::generate(MIN_RSA_BITS).unwrap();
        assert!(
            signer
                .public_key_pem()
                .unwrap()
                .contains("BEGIN RSA PUBLIC KEY")
        );

        let sig = signer.sign(b"message").unwrap();
        assert!(signer.verify(b"message", &sig).unwrap());
        assert!(!signer.verify(b"tampered", &sig).unwrap());
    }

    #[test]
    fn should_verify_with_peer_public_key() {
        let signer = RsaSigner::generate(MIN_RSA_BITS).unwrap();
        let pem = signer.public_key_pem().unwrap();
        let verifier = RsaVerifier::from_public_key_pem(&pem).unwrap();

        let sig = signer.sign(b"payload").unwrap();
        assert!(verifier.verify(b"payload", &sig).unwrap());
        assert!(!verifier.verify(b"payload!", &sig).unwrap());
    }

    #[test]
    fn should_treat_malformed_signature_as_mismatch() {
        let signer = RsaSigner::generate(MIN_RSA_BITS).unwrap();
        // 攻击者可控的畸形签名不应变成 500
        assert!(!signer.verify(b"message", b"garbage").unwrap());
        assert!(!signer.verify(b"message", &[]).unwrap());
    }

    #[test]
    fn should_roundtrip_rsa_pss() {
        let signer = RsaSigner::generate(MIN_RSA_BITS).unwrap();
        let verifier = signer.verifier();
        let sig = signer.sign_pss(b"message").unwrap();
        assert!(verifier.verify_pss(b"message", &sig).unwrap());
        assert!(!verifier.verify_pss(b"tampered", &sig).unwrap());
        // PKCS#1 v1.5 的验签器不能验 PSS 签名（防跨方案混淆）
        assert!(!verifier.verify(b"message", &sig).unwrap());
    }

    #[test]
    fn should_reject_weak_rsa_bits() {
        assert!(RsaSigner::generate(512).is_err());
        assert!(RsaSigner::generate(0).is_err());
        assert!(RsaSigner::generate(MAX_RSA_BITS + 1).is_err());
    }

    #[test]
    fn should_reject_invalid_public_key_pem() {
        assert!(RsaVerifier::from_public_key_pem("not a pem").is_err());
        assert!(RsaVerifier::from_public_key_pem("").is_err());
    }

    #[test]
    fn should_compare_in_constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn should_keep_hmac_config_empty_as_none() {
        let section = SecuritySection {
            hmac: HmacSecurity { key: String::new() },
            ..SecuritySection::default()
        };
        assert!(HmacSigner::from_config(&section).unwrap().is_none());
    }
}
