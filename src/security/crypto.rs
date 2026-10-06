#![allow(deprecated)] // aes-gcm 0.10 / generic-array 0.14 的 from_slice 尚未迁移

//! 加密工具（文档 三·16）：AES-GCM / RSA / HMAC / 摘要 / 安全随机数，
//! 统一入口，避免各应用各引一套加密库。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{AppError, AppResult};

type HmacSha256 = Hmac<Sha256>;

// ---------- 随机数 ----------

/// 安全随机字节（系统熵源）
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    buf
}

// ---------- 摘要 ----------

/// SHA-256 摘要（hex 输出）
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex_encode(&digest)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// hex 解码（大小写均可）；非法输入返回 None
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

// ---------- HMAC ----------

/// HMAC-SHA256 签名（hex 输出）
pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> AppResult<String> {
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(key)
        .map_err(|e| AppError::internal(format!("hmac key error: {e}")))?;
    mac.update(data);
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

/// 常量时间校验 HMAC（比较经 `subtle::ConstantTimeEq`，与输入内容无关）
pub fn hmac_sha256_verify(key: &[u8], data: &[u8], expected_hex: &str) -> AppResult<bool> {
    let Some(expected) = hex_decode(expected_hex) else {
        // expected 不是合法 hex：直接判 false（与长度不等同路径）
        return Ok(false);
    };
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(key)
        .map_err(|e| AppError::internal(format!("hmac key error: {e}")))?;
    mac.update(data);
    let actual = mac.finalize().into_bytes();
    // Iterator::all 首个失配即短路，时序随首个差异字节的位置变化——
    // 必须用常量时间比较，否则给侧信道留口子
    Ok(bool::from(actual.as_slice().ct_eq(expected.as_slice())))
}

// ---------- AES-256-GCM ----------

/// AES-256-GCM 加密：随机 12 字节 nonce 前置，输出 base64(nonce + ciphertext)。
/// `key` 必须 32 字节（通常为派生/保管的原始密钥 hex/base64 解码后）。
pub fn aes_gcm_encrypt(key: &[u8], plaintext: &[u8]) -> AppResult<String> {
    let cipher = cipher_for(key)?;
    let nonce_bytes = random_bytes(12);
    let nonce = Nonce::from_slice(&nonce_bytes); // 12 字节
    let ciphertext = cipher
        .encrypt(nonce, Payload { msg: plaintext, aad: &[] })
        .map_err(|e| AppError::internal(format!("aes-gcm encrypt failed: {e}")))?;

    let mut out = nonce_bytes;
    out.extend_from_slice(&ciphertext);
    Ok(BASE64.encode(out))
}

/// AES-256-GCM 解密（[`aes_gcm_encrypt`] 的逆操作）
pub fn aes_gcm_decrypt(key: &[u8], encoded: &str) -> AppResult<Vec<u8>> {
    let data = BASE64
        .decode(encoded)
        .map_err(|e| AppError::internal(format!("aes-gcm input is not base64: {e}")))?;
    if data.len() < 13 {
        return Err(AppError::internal("aes-gcm input too short"));
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let cipher = cipher_for(key)?;
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ciphertext, aad: &[] })
        .map_err(|_| AppError::internal("aes-gcm decrypt failed (wrong key or tampered)"))
}

fn cipher_for(key: &[u8]) -> AppResult<Aes256Gcm> {
    if key.len() != 32 {
        return Err(AppError::internal(format!(
            "aes-256-gcm key must be 32 bytes, got {}",
            key.len()
        )));
    }
    Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)))
}

// ---------- RSA ----------

pub use rsa_impl::{rsa_decrypt, rsa_encrypt, rsa_generate, rsa_sign, rsa_verify, RsaKeypair};

mod rsa_impl {
    use base64::Engine as _;

    use super::{AppError, AppResult, BASE64};
    use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
    use rsa::signature::{SignatureEncoding, Signer, Verifier};
    use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};

    /// 生成 RSA 密钥对（PKCS#8 PEM）
    pub struct RsaKeypair {
        pub private_pem: String,
        pub public_pem: String,
    }

    pub fn rsa_generate(bits: usize) -> AppResult<RsaKeypair> {
        use rand_core::OsRng as RsaOsRng;
        // <2048 位可被现代算力分解，不提供生成弱密钥的口子
        if bits < 2048 {
            return Err(AppError::internal(
                "rsa_generate: bits must be >= 2048",
            ));
        }
        let mut rng = RsaOsRng;
        let private = RsaPrivateKey::new(&mut rng, bits)
            .map_err(|e| AppError::internal(format!("rsa keygen failed: {e}")))?;
        let public = RsaPublicKey::from(&private);
            use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
        Ok(RsaKeypair {
            private_pem: private
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                .map_err(|e| AppError::internal(format!("rsa pem encode failed: {e}")))?
                .to_string(),
            public_pem: public
                .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
                .map_err(|e| AppError::internal(format!("rsa pem encode failed: {e}")))?,
        })
    }

    fn load_public(public_pem: &str) -> AppResult<RsaPublicKey> {
        use rsa::pkcs8::DecodePublicKey;
        RsaPublicKey::from_public_key_pem(public_pem)
            .map_err(|e| AppError::internal(format!("bad rsa public pem: {e}")))
    }

    fn load_private(private_pem: &str) -> AppResult<RsaPrivateKey> {
        use rsa::pkcs8::DecodePrivateKey;
        RsaPrivateKey::from_pkcs8_pem(private_pem)
            .map_err(|e| AppError::internal(format!("bad rsa private pem: {e}")))
    }

    /// RSA 公钥加密（OAEP-SHA256，抗 Bleichenbacher 填充预言；PKCS#1 v1.5 加密
    /// 已被证明不可安全实现），输出 base64
    pub fn rsa_encrypt(public_pem: &str, plaintext: &[u8]) -> AppResult<String> {
        let mut rng = rand_core::OsRng;
        let encrypted = load_public(public_pem)?
            .encrypt(&mut rng, Oaep::new::<sha2::Sha256>(), plaintext)
            .map_err(|e| AppError::internal(format!("rsa encrypt failed: {e}")))?;
        Ok(BASE64.encode(encrypted))
    }

    /// RSA 私钥解密（[`rsa_encrypt`] 的逆操作）
    pub fn rsa_decrypt(private_pem: &str, encoded: &str) -> AppResult<Vec<u8>> {
        let data = BASE64
            .decode(encoded)
            .map_err(|e| AppError::internal(format!("rsa input is not base64: {e}")))?;
        load_private(private_pem)?
            .decrypt(Oaep::new::<sha2::Sha256>(), &data)
            .map_err(|e| AppError::internal(format!("rsa decrypt failed: {e}")))
    }

    /// RSA-SHA256 签名（base64）
    pub fn rsa_sign(private_pem: &str, data: &[u8]) -> AppResult<String> {
        let key = load_private(private_pem)?;
        let signing = SigningKey::<sha2::Sha256>::new(key);
        Ok(BASE64.encode(signing.sign(data).to_bytes()))
    }

    /// RSA-SHA256 验签
    pub fn rsa_verify(public_pem: &str, data: &[u8], signature_b64: &str) -> AppResult<bool> {
        let signature = BASE64
            .decode(signature_b64)
            .map_err(|e| AppError::internal(format!("signature is not base64: {e}")))?;
        let verifying = VerifyingKey::<sha2::Sha256>::new(load_public(public_pem)?);
        Ok(verifying
            .verify(data, &Signature::try_from(signature.as_slice()).map_err(|e| AppError::internal(format!("bad signature: {e}")))?)
            .is_ok())
    }
}

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_and_hmac() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let sig = hmac_sha256_hex(b"k", b"data").unwrap();
        assert!(hmac_sha256_verify(b"k", b"data", &sig).unwrap());
        assert!(!hmac_sha256_verify(b"k2", b"data", &sig).unwrap());
    }

    #[test]
    fn aes_gcm_roundtrip() {
        let key = random_bytes(32);
        let encrypted = aes_gcm_encrypt(&key, "secret 你好".as_bytes()).unwrap();
        assert_eq!(aes_gcm_decrypt(&key, &encrypted).unwrap(), "secret 你好".as_bytes());
        assert!(aes_gcm_decrypt(&key, &encrypted[..encrypted.len() - 4]).is_err());
    }

    #[test]
    fn rsa_roundtrip() {
        let kp = rsa_impl::rsa_generate(2048).unwrap();
        let encrypted = rsa_impl::rsa_encrypt(&kp.public_pem, b"hello").unwrap();
        assert_eq!(rsa_impl::rsa_decrypt(&kp.private_pem, &encrypted).unwrap(), b"hello");
        let sig = rsa_impl::rsa_sign(&kp.private_pem, b"msg").unwrap();
        assert!(rsa_impl::rsa_verify(&kp.public_pem, b"msg", &sig).unwrap());
        assert!(!rsa_impl::rsa_verify(&kp.public_pem, b"msg2", &sig).unwrap());
    }
}
