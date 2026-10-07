#![allow(deprecated)] // aes-gcm 0.10 / generic-array 0.14 的 from_slice 尚未迁移

//! 加密工具（文档 三·16）：AES-GCM / RSA / HMAC / 摘要 / 安全随机数，
//! 统一入口，避免各应用各引一套加密库。

use aes_gcm::aead::{Aead, KeyInit, Payload}; // 引入 AEAD 加解密 trait、密钥初始化与附加数据载体
use aes_gcm::{Aes256Gcm, Key, Nonce}; // 引入 AES-256-GCM 算法、密钥与 nonce 类型
use base64::Engine; // 引入 base64 编解码 trait（提供 encode/decode 方法）
use hmac::{Hmac, Mac}; // 引入 HMAC 泛型与 Mac trait（update/finalize）
use rand_core::{OsRng, RngCore}; // 引入系统熵源 OsRng 与随机字节填充 trait
use sha2::{Digest, Sha256}; // 引入 SHA-256 摘要算法与 Digest trait
use subtle::ConstantTimeEq; // 引入常量时间比较 trait，抵御时序侧信道

use crate::error::{AppError, AppResult}; // 引入框架统一错误类型与结果别名

type HmacSha256 = Hmac<Sha256>; // 定义 HMAC-SHA256 类型别名，简化后续书写

// ---------- 随机数 ----------

/// 安全随机字节（系统熵源）
pub fn random_bytes(n: usize) -> Vec<u8> { // 生成 n 字节密码学安全随机数
    let mut buf = vec![0u8; n]; // 预分配 n 字节缓冲
    OsRng.fill_bytes(&mut buf); // 用系统熵源填充缓冲（阻塞式安全随机）
    buf // 返回随机字节
}

// ---------- 摘要 ----------

/// SHA-256 摘要（hex 输出）
pub fn sha256_hex(data: &[u8]) -> String { // 计算数据的 SHA-256 摘要并转 hex
    let digest = Sha256::digest(data); // 计算 32 字节摘要
    hex_encode(&digest) // 把摘要编码为 hex 字符串
}

pub fn hex_encode(bytes: &[u8]) -> String { // 把字节切片编码为小写 hex 字符串
    const HEX: &[u8; 16] = b"0123456789abcdef"; // hex 字符表（小写）
    let mut out = String::with_capacity(bytes.len() * 2); // 预分配 2 倍长度的输出缓冲
    for b in bytes { // 逐字节处理
        out.push(HEX[(b >> 4) as usize] as char); // 取高 4 位映射为 hex 字符
        out.push(HEX[(b & 0x0f) as usize] as char); // 取低 4 位映射为 hex 字符
    }
    out // 返回 hex 字符串
}

/// hex 解码（大小写均可）；非法输入返回 None
pub fn hex_decode(s: &str) -> Option<Vec<u8>> { // 把 hex 字符串解码为字节
    if s.len() % 2 != 0 { // hex 长度必须为偶数（每两字符一字节）
        return None; // 奇数长度直接判定非法
    }
    (0..s.len() / 2) // 按字节数迭代
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()) // 每两字符按 16 进制解析，失败得 None
        .collect() // 收集为 Option<Vec<u8>>（任一失败则整体 None）
}

// ---------- HMAC ----------

/// HMAC-SHA256 签名（hex 输出）
pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> AppResult<String> { // 用密钥对数据做 HMAC-SHA256 签名
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(key) // 用任意长度密钥初始化 HMAC
        .map_err(|e| AppError::internal(format!("hmac key error: {e}")))?; // 密钥非法则转为内部错误
    mac.update(data); // 喂入待签名数据
    Ok(hex_encode(&mac.finalize().into_bytes())) // 完成签名并 hex 编码返回
}

/// 常量时间校验 HMAC（比较经 `subtle::ConstantTimeEq`，与输入内容无关）
pub fn hmac_sha256_verify(key: &[u8], data: &[u8], expected_hex: &str) -> AppResult<bool> { // 常量时间校验 HMAC 签名
    let Some(expected) = hex_decode(expected_hex) else { // 先把期望签名从 hex 解码
        // expected 不是合法 hex：直接判 false（与长度不等同路径）
        return Ok(false); // 非法 hex 视为校验不通过
    };
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(key) // 用密钥初始化 HMAC
        .map_err(|e| AppError::internal(format!("hmac key error: {e}")))?; // 密钥非法则转为内部错误
    mac.update(data); // 喂入待校验数据
    let actual = mac.finalize().into_bytes(); // 计算实际签名
    // Iterator::all 首个失配即短路，时序随首个差异字节的位置变化——
    // 必须用常量时间比较，否则给侧信道留口子
    Ok(bool::from(actual.as_slice().ct_eq(expected.as_slice()))) // 常量时间比较，避免时序侧信道泄露
}

// ---------- AES-256-GCM ----------

/// AES-256-GCM 加密：随机 12 字节 nonce 前置，输出 base64(nonce + ciphertext)。
/// `key` 必须 32 字节（通常为派生/保管的原始密钥 hex/base64 解码后）。
pub fn aes_gcm_encrypt(key: &[u8], plaintext: &[u8]) -> AppResult<String> { // AES-256-GCM 认证加密
    let cipher = cipher_for(key)?; // 由 32 字节密钥构造密码器
    let nonce_bytes = random_bytes(12); // 生成随机 12 字节 nonce（GCM 标准长度）
    let nonce = Nonce::from_slice(&nonce_bytes); // 12 字节
    let ciphertext = cipher // 开始加密
        .encrypt(nonce, Payload { msg: plaintext, aad: &[] }) // 加密明文（无附加认证数据）
        .map_err(|e| AppError::internal(format!("aes-gcm encrypt failed: {e}")))?; // 加密失败转为内部错误

    let mut out = nonce_bytes; // 输出缓冲以 nonce 开头
    out.extend_from_slice(&ciphertext); // 追加密文（含 GCM tag）
    Ok(BASE64.encode(out)) // 整体 base64 编码后返回
}

/// AES-256-GCM 解密（[`aes_gcm_encrypt`] 的逆操作）
pub fn aes_gcm_decrypt(key: &[u8], encoded: &str) -> AppResult<Vec<u8>> { // AES-256-GCM 认证解密
    let data = BASE64 // 先对输入做 base64 解码
        .decode(encoded) // 执行解码
        .map_err(|e| AppError::internal(format!("aes-gcm input is not base64: {e}")))?; // 非法 base64 报内部错误
    if data.len() < 13 { // 至少需 12 字节 nonce + 1 字节密文
        return Err(AppError::internal("aes-gcm input too short")); // 长度不足直接报错
    }
    let (nonce_bytes, ciphertext) = data.split_at(12); // 前 12 字节为 nonce，其余为密文
    let cipher = cipher_for(key)?; // 由密钥构造密码器
    cipher // 开始解密
        .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ciphertext, aad: &[] }) // 解密并校验 GCM tag
        .map_err(|_| AppError::internal("aes-gcm decrypt failed (wrong key or tampered)")) // 失败即密钥错误或数据被篡改
}

fn cipher_for(key: &[u8]) -> AppResult<Aes256Gcm> { // 由原始密钥构造 AES-256-GCM 密码器
    if key.len() != 32 { // AES-256 要求密钥恰为 32 字节
        return Err(AppError::internal(format!( // 长度不符则报内部错误
            "aes-256-gcm key must be 32 bytes, got {}",
            key.len()
        )));
    }
    Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key))) // 由密钥切片构造密码器
}

// ---------- RSA ----------

pub use rsa_impl::{rsa_decrypt, rsa_encrypt, rsa_generate, rsa_sign, rsa_verify, RsaKeypair}; // 重新导出 RSA 子模块的公开 API

mod rsa_impl { // RSA 实现子模块（隐藏内部依赖细节）
    use base64::Engine as _; // 以 trait 方式引入 base64 引擎（匿名导入避免命名冲突）

    use super::{AppError, AppResult, BASE64}; // 复用父模块的错误类型与 base64 引擎
    use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey}; // 引入 PKCS#1 v1.5 签名相关类型
    use rsa::signature::{SignatureEncoding, Signer, Verifier}; // 引入签名编码与签名/验签 trait
    use rsa::{Oaep, RsaPrivateKey, RsaPublicKey}; // 引入 OAEP 填充与 RSA 公私钥类型

    /// 生成 RSA 密钥对（PKCS#8 PEM）
    pub struct RsaKeypair { // RSA 密钥对容器（PEM 文本）
        pub private_pem: String, // 私钥 PKCS#8 PEM
        pub public_pem: String, // 公钥 SPKI PEM
    }

    pub fn rsa_generate(bits: usize) -> AppResult<RsaKeypair> { // 生成指定位数的 RSA 密钥对
        use rand_core::OsRng as RsaOsRng; // 局部引入系统熵源并改名，避免与父模块 OsRng 混淆
        // <2048 位可被现代算力分解，不提供生成弱密钥的口子
        if bits < 2048 { // 拒绝弱密钥位数
            return Err(AppError::internal(
                "rsa_generate: bits must be >= 2048",
            ));
        }
        let mut rng = RsaOsRng; // 取得随机数发生器实例
        let private = RsaPrivateKey::new(&mut rng, bits) // 生成私钥（大素数运算，较慢）
            .map_err(|e| AppError::internal(format!("rsa keygen failed: {e}")))?; // 失败转为内部错误
        let public = RsaPublicKey::from(&private); // 由私钥推导公钥
            use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey}; // 引入 PEM 编码 trait
        Ok(RsaKeypair { // 组装密钥对返回
            private_pem: private // 私钥 PEM 编码
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF) // 编码为 PKCS#8 PEM（LF 换行）
                .map_err(|e| AppError::internal(format!("rsa pem encode failed: {e}")))? // 编码失败转内部错误
                .to_string(), // 转为 String
            public_pem: public // 公钥 PEM 编码
                .to_public_key_pem(rsa::pkcs8::LineEnding::LF) // 编码为 SPKI PEM（LF 换行）
                .map_err(|e| AppError::internal(format!("rsa pem encode failed: {e}")))?, // 编码失败转内部错误
        })
    }

    fn load_public(public_pem: &str) -> AppResult<RsaPublicKey> { // 从 PEM 解析公钥
        use rsa::pkcs8::DecodePublicKey; // 引入 PEM 解码 trait
        RsaPublicKey::from_public_key_pem(public_pem) // 解析公钥 PEM
            .map_err(|e| AppError::internal(format!("bad rsa public pem: {e}"))) // 解析失败转内部错误
    }

    fn load_private(private_pem: &str) -> AppResult<RsaPrivateKey> { // 从 PEM 解析私钥
        use rsa::pkcs8::DecodePrivateKey; // 引入 PKCS#8 解码 trait
        RsaPrivateKey::from_pkcs8_pem(private_pem) // 解析私钥 PKCS#8 PEM
            .map_err(|e| AppError::internal(format!("bad rsa private pem: {e}"))) // 解析失败转内部错误
    }

    /// RSA 公钥加密（OAEP-SHA256，抗 Bleichenbacher 填充预言；PKCS#1 v1.5 加密
    /// 已被证明不可安全实现），输出 base64
    pub fn rsa_encrypt(public_pem: &str, plaintext: &[u8]) -> AppResult<String> { // 用 RSA 公钥加密（OAEP-SHA256）
        let mut rng = rand_core::OsRng; // 取得系统随机源（OAEP 需要随机填充）
        let encrypted = load_public(public_pem)? // 解析公钥
            .encrypt(&mut rng, Oaep::new::<sha2::Sha256>(), plaintext) // 以 OAEP-SHA256 加密明文
            .map_err(|e| AppError::internal(format!("rsa encrypt failed: {e}")))?; // 加密失败转内部错误
        Ok(BASE64.encode(encrypted)) // 密文 base64 编码返回
    }

    /// RSA 私钥解密（[`rsa_encrypt`] 的逆操作）
    pub fn rsa_decrypt(private_pem: &str, encoded: &str) -> AppResult<Vec<u8>> { // 用 RSA 私钥解密（OAEP-SHA256）
        let data = BASE64 // 对输入做 base64 解码
            .decode(encoded) // 执行解码
            .map_err(|e| AppError::internal(format!("rsa input is not base64: {e}")))?; // 非法 base64 转内部错误
        load_private(private_pem)? // 解析私钥
            .decrypt(Oaep::new::<sha2::Sha256>(), &data) // 以 OAEP-SHA256 解密
            .map_err(|e| AppError::internal(format!("rsa decrypt failed: {e}"))) // 解密失败转内部错误
    }

    /// RSA-SHA256 签名（base64）
    pub fn rsa_sign(private_pem: &str, data: &[u8]) -> AppResult<String> { // 用 RSA 私钥对数据签名
        let key = load_private(private_pem)?; // 解析私钥
        let signing = SigningKey::<sha2::Sha256>::new(key); // 构造 SHA-256 签名密钥
        Ok(BASE64.encode(signing.sign(data).to_bytes())) // 签名并 base64 编码返回
    }

    /// RSA-SHA256 验签
    pub fn rsa_verify(public_pem: &str, data: &[u8], signature_b64: &str) -> AppResult<bool> { // 用 RSA 公钥验签
        let signature = BASE64 // 对签名做 base64 解码
            .decode(signature_b64) // 执行解码
            .map_err(|e| AppError::internal(format!("signature is not base64: {e}")))?; // 非法 base64 转内部错误
        let verifying = VerifyingKey::<sha2::Sha256>::new(load_public(public_pem)?); // 构造 SHA-256 验签密钥
        Ok(verifying // 执行验签
            .verify(data, &Signature::try_from(signature.as_slice()).map_err(|e| AppError::internal(format!("bad signature: {e}")))?) // 解析签名字节并验证
            .is_ok()) // 验证成功即 true
    }
}

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD; // 统一使用的标准 base64 引擎

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
