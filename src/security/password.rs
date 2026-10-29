//! 密码哈希（Argon2id）。
//!
//! 见 `docs/11` §6：**禁止**明文、MD5、SHA1、无盐哈希；登录失败统一提示以防用户枚举。
//!
//! # 三道 fail-closed 闸门
//!
//! 1. **参数有下限**：[`Argon2Params::new`] 拒绝弱参数，不存在「配了但等于没配」；
//! 2. **校验有上限**：从库里读出的 PHC 串若声明了天文数字的 `m` / `t`（脏数据、
//!    迁移残留、被篡改），直接按不匹配处理，不让一次登录申请 4 GiB 内存；
//! 3. **等时失败**：用户不存在、哈希格式不认识时也会跑一次等价的哈希计算
//!    （[`verify_password_or_dummy`]），否则响应时间就是账号是否存在的探针。

use std::fmt;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version, password_hash::rand_core::OsRng};

use crate::config::sections::security::{
    MAX_ARGON2_MEMORY_KIB, MIN_ARGON2_MEMORY_KIB, MIN_ARGON2_TIME_COST, PasswordSecurity,
};
use crate::errors::{AppError, AppResult};

/// 密码长度上限（字节）。
///
/// NIST SP 800-63B 只要求支持 64 字节以上；这里给到 1024 是为了挡住
/// 「用 2 MB 密码打登录接口」——Argon2 对密码长度是线性开销，配合限流
/// 仍足以放大 CPU 消耗。
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Argon2id 迭代次数上限（防脏数据把单次校验拖成秒级）。
pub const MAX_ARGON2_TIME_COST: u32 = 64;

/// Argon2id 并行度上限。
pub const MAX_ARGON2_PARALLELISM: u32 = 64;

/// 唯一接受的算法标识。
const ARGON2ID_IDENT: &str = "argon2id";

/// Argon2id 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    /// 内存开销（KiB）。
    pub memory_cost: u32,
    /// 迭代次数。
    pub time_cost: u32,
    /// 并行度。
    pub parallelism: u32,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self {
            memory_cost: 19_456,
            time_cost: 2,
            parallelism: 1,
        }
    }
}

impl Argon2Params {
    /// 构造并校验参数。
    ///
    /// 下限与 `[security].password.*` 的启动期校验一致——**不校验下限的构造函数
    /// 等于给弱参数开了一条绕过启动校验的路**（`Argon2Params::new(8, 1, 1)`
    /// 曾是最顺手的写法）。
    ///
    /// # Errors
    /// 参数越界时返回 `10001`。
    pub fn new(memory_cost: u32, time_cost: u32, parallelism: u32) -> AppResult<Self> {
        let params = Self {
            memory_cost,
            time_cost,
            parallelism,
        };
        params.validate()?;
        Ok(params)
    }

    /// 由 `[security].password` 构造。
    ///
    /// # Errors
    /// 参数越界时返回 `10001`。
    pub fn from_config(section: &PasswordSecurity) -> AppResult<Self> {
        Self::new(section.memory_cost, section.time_cost, section.parallelism)
    }

    /// 参数范围校验。
    ///
    /// # Errors
    /// 内存开销 / 迭代次数 / 并行度越界时返回 `10001`。
    pub fn validate(&self) -> AppResult<()> {
        if !(MIN_ARGON2_MEMORY_KIB..=MAX_ARGON2_MEMORY_KIB).contains(&self.memory_cost) {
            return Err(AppError::invalid_param(format!(
                "Argon2id memory_cost 必须在 {MIN_ARGON2_MEMORY_KIB}..={MAX_ARGON2_MEMORY_KIB} KiB，\
                 当前 {} KiB",
                self.memory_cost
            )));
        }
        if !(MIN_ARGON2_TIME_COST..=MAX_ARGON2_TIME_COST).contains(&self.time_cost) {
            return Err(AppError::invalid_param(format!(
                "Argon2id time_cost 必须在 {MIN_ARGON2_TIME_COST}..={MAX_ARGON2_TIME_COST}，当前 {}",
                self.time_cost
            )));
        }
        if !(1..=MAX_ARGON2_PARALLELISM).contains(&self.parallelism) {
            return Err(AppError::invalid_param(format!(
                "Argon2id parallelism 必须在 1..={MAX_ARGON2_PARALLELISM}，当前 {}",
                self.parallelism
            )));
        }
        Ok(())
    }

    /// 转成 `argon2::Params`。
    fn argon2_params(&self) -> AppResult<Params> {
        Params::new(
            self.memory_cost,
            self.time_cost,
            self.parallelism,
            Some(Params::DEFAULT_OUTPUT_LEN),
        )
        .map_err(|e| AppError::internal(anyhow::anyhow!("invalid argon2 params: {e}")))
    }

    /// 按本参数构造 Argon2id 实例。
    fn hasher(&self) -> AppResult<Argon2<'static>> {
        Ok(Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            self.argon2_params()?,
        ))
    }
}

/// 密码校验结果。
///
/// 对外**必须统一提示**（「用户名或密码错误」），但内部要能区分——否则
/// 「用户存在但哈希格式坏了」和「密码输错了」在日志里长得一模一样，
/// 迁移老系统（bcrypt → Argon2id）时全站登录失败会完全查不出原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyOutcome {
    /// 密码匹配。
    Match,
    /// 密码不匹配。
    Mismatch,
    /// 哈希格式不认识（算法非 Argon2id / 参数越界 / PHC 串非法）。
    ///
    /// **对外仍按「不匹配」处理**，只是会打一条 warn。
    UnsupportedFormat,
}

impl VerifyOutcome {
    /// 是否匹配（`Mismatch` 与 `UnsupportedFormat` 都是 `false`）。
    #[must_use]
    pub fn is_match(self) -> bool {
        matches!(self, Self::Match)
    }
}

impl fmt::Display for VerifyOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::UnsupportedFormat => "unsupported-format",
        })
    }
}

/// 取密码字节，并挡住超长输入。
fn password_bytes(password: &str) -> AppResult<&[u8]> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::invalid_param(format!(
            "密码长度超过 {MAX_PASSWORD_BYTES} 字节上限"
        )));
    }
    Ok(password.as_bytes())
}

/// 哈希密码（Argon2id + 随机盐）。
///
/// # Errors
/// 密码超长返回 `10001`；参数非法或底层哈希失败时返回 `50001`。
pub fn hash_password(password: &str, params: &Argon2Params) -> AppResult<String> {
    let password = password_bytes(password)?;
    let argon2 = params.hasher()?;
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2
        .hash_password(password, &salt)
        .map_err(|e| AppError::internal(anyhow::anyhow!("hash password failed: {e}")))?;
    Ok(hash.to_string())
}

/// 校验密码与哈希是否匹配。
///
/// 参数从 PHC 串里解析（`argon2` crate 的 `PasswordVerifier` 会按串里的
/// `m` / `t` / `p` 重算），因此改参数不会让历史哈希失效。
///
/// # Errors
/// 密码超长返回 `10001`；仅在校验库本身出错时返回 `50001`
/// （格式不认识返回 [`VerifyOutcome::UnsupportedFormat`]，不是错误）。
pub fn verify_password(password: &str, hash: &str) -> AppResult<VerifyOutcome> {
    let password = password_bytes(password)?;

    let parsed = match PasswordHash::new(hash) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(error = %error, "密码哈希格式非法，按不匹配处理");
            burn(password, &Argon2Params::default())?;
            return Ok(VerifyOutcome::UnsupportedFormat);
        }
    };

    // 只接受 Argon2id：argon2i / argon2d / 其他算法一律拒绝（防算法降级）。
    if parsed.algorithm.as_str() != ARGON2ID_IDENT {
        tracing::warn!(
            algorithm = %parsed.algorithm,
            "密码哈希算法不是 Argon2id，拒绝校验"
        );
        burn(password, &Argon2Params::default())?;
        return Ok(VerifyOutcome::UnsupportedFormat);
    }

    // 上限校验：库里被写入 `m=4194304` 的串会让每次登录申请 4 GiB → OOM。
    if !params_within_limits(&parsed) {
        tracing::warn!("密码哈希参数超出上限，拒绝校验（防 OOM）");
        burn(password, &Argon2Params::default())?;
        return Ok(VerifyOutcome::UnsupportedFormat);
    }

    Ok(match Argon2::default().verify_password(password, &parsed) {
        Ok(()) => VerifyOutcome::Match,
        Err(_) => VerifyOutcome::Mismatch,
    })
}

/// 等时校验：`hash` 为 `None`（用户不存在）时也消耗一次等价的哈希开销。
///
/// 登录接口应当**始终**走这个入口：直接 `if let Some(hash) = user { ... }`
/// 会让「用户不存在」比「密码错误」快一个数量级，响应时间就是账号枚举器。
///
/// # Errors
/// 密码超长返回 `10001`；底层哈希失败返回 `50001`。
pub fn verify_password_or_dummy(
    password: &str,
    hash: Option<&str>,
    params: &Argon2Params,
) -> AppResult<bool> {
    match hash {
        Some(hash) => Ok(verify_password(password, hash)?.is_match()),
        None => {
            burn(password_bytes(password)?, params)?;
            Ok(false)
        }
    }
}

/// 跑一次与校验等价的哈希计算并丢弃结果（用于抹平时间差）。
fn burn(password: &[u8], params: &Argon2Params) -> AppResult<()> {
    let argon2 = params.hasher()?;
    let salt = SaltString::generate(&mut OsRng);
    // 结果丢弃；失败也不影响「不匹配」的结论。
    let _ = argon2.hash_password(password, &salt);
    Ok(())
}

/// PHC 串里声明的参数是否在允许范围内。
fn params_within_limits(parsed: &PasswordHash<'_>) -> bool {
    let Some(memory) = parsed
        .params
        .get_str("m")
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return false;
    };
    let Some(time) = parsed
        .params
        .get_str("t")
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return false;
    };
    // `p`（并行度）同样要封顶：脏数据里写 `p=1024` 会让 argon2 申请大量 lane，
    // 与 `m` / `t` 是同一类放大面。
    let parallelism = parsed
        .params
        .get_str("p")
        .and_then(|value| value.parse::<u32>().ok());
    let parallelism_ok = parallelism.is_none_or(|p| (1..=MAX_ARGON2_PARALLELISM).contains(&p));

    (MIN_ARGON2_MEMORY_KIB..=MAX_ARGON2_MEMORY_KIB).contains(&memory)
        && time <= MAX_ARGON2_TIME_COST
        && parallelism_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用参数（仍满足下限，只是内存小一点让测试跑得快）。
    fn fast_params() -> Argon2Params {
        Argon2Params::new(8 * 1024, 2, 1).unwrap()
    }

    #[test]
    fn should_roundtrip_hash_and_verify() {
        let params = fast_params();
        let hash = hash_password("correct horse battery staple", &params).unwrap();

        assert!(hash.starts_with("$argon2id$"), "应使用 Argon2id：{hash}");
        assert!(
            verify_password("correct horse battery staple", &hash)
                .unwrap()
                .is_match()
        );
        assert_eq!(
            verify_password("wrong password", &hash).unwrap(),
            VerifyOutcome::Mismatch
        );
    }

    #[test]
    fn should_salt_each_hash() {
        let params = fast_params();
        let a = hash_password("same", &params).unwrap();
        let b = hash_password("same", &params).unwrap();
        assert_ne!(a, b, "每次哈希应使用不同盐");
    }

    #[test]
    fn should_treat_malformed_hash_as_mismatch() {
        let outcome = verify_password("x", "not-a-hash").unwrap();
        assert_eq!(outcome, VerifyOutcome::UnsupportedFormat);
        assert!(!outcome.is_match());
    }

    #[test]
    fn should_reject_weak_argon2_params() {
        // 弱参数曾被当成「最顺手的写法」（框架自己的测试就写过 8KiB/1 次）
        assert!(Argon2Params::new(1024, 2, 1).is_err());
        assert!(Argon2Params::new(19_456, 1, 1).is_err());
        assert!(Argon2Params::new(19_456, 2, 0).is_err());
        assert!(Argon2Params::new(MAX_ARGON2_MEMORY_KIB + 1, 2, 1).is_err());
        assert!(Argon2Params::new(19_456, MAX_ARGON2_TIME_COST + 1, 1).is_err());
        assert!(Argon2Params::default().validate().is_ok());
    }

    #[test]
    fn should_reject_non_argon2id_hash() {
        // argon2i 的哈希不能被接受（防算法降级）
        let argon2i = Argon2::new(
            Algorithm::Argon2i,
            Version::V0x13,
            Params::new(8 * 1024, 2, 1, None).unwrap(),
        );
        let salt = SaltString::generate(&mut OsRng);
        let hash = argon2i.hash_password(b"secret", &salt).unwrap().to_string();

        assert_eq!(
            verify_password("secret", &hash).unwrap(),
            VerifyOutcome::UnsupportedFormat
        );
    }

    #[test]
    fn should_reject_hash_with_absurd_memory_cost() {
        // 模拟被写入脏数据 / 被篡改的 PHC 串：m=4194304（4 GiB）
        let hash = "$argon2id$v=19$m=4194304,t=2,p=1$c29tZXNhbHRzYWx0$\
                    3g4Z0Y3Yw7oQ9m7oQ9m7oQ9m7oQ9m7oQ9m7oQ9m7oQ";
        assert_eq!(
            verify_password("secret", hash).unwrap(),
            VerifyOutcome::UnsupportedFormat
        );
    }

    #[test]
    fn should_reject_oversized_password() {
        let params = fast_params();
        let long = "a".repeat(MAX_PASSWORD_BYTES + 1);
        assert!(hash_password(&long, &params).is_err());
        // 校验侧同样挡住：不因为「库里已有哈希」就放行超长输入
        let hash = hash_password("ok", &params).unwrap();
        assert!(verify_password(&long, &hash).is_err());
    }

    #[test]
    fn should_return_false_for_unknown_user() {
        let params = fast_params();
        assert!(!verify_password_or_dummy("whatever", None, &params).unwrap());
        let hash = hash_password("secret", &params).unwrap();
        assert!(verify_password_or_dummy("secret", Some(&hash), &params).unwrap());
        assert!(!verify_password_or_dummy("wrong", Some(&hash), &params).unwrap());
    }

    #[test]
    fn should_build_params_from_config() {
        let section = PasswordSecurity::default();
        let params = Argon2Params::from_config(&section).unwrap();
        assert_eq!(params, Argon2Params::default());
    }
}
