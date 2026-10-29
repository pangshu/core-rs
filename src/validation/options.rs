//! 校验选项：`[validation]` 配置段在请求路径上的唯一落点。
//!
//! `validate()` 与 [`Validated<T>`](crate::validation::Validated) 都是**无状态入口**
//! （前者是自由函数，后者是泛型提取器，拿不到 `AppState`），因此选项走**进程级**
//! 单元格：启动 / 配置热更新时由 [`crate::config::ConfigHandle`] 写入，请求路径上
//! 只做一次无锁读。
//!
//! # 为什么不用 `AppState`
//!
//! `Validated<T>` 对任意 `S` 实现 `FromRequest`（与 `http::Json` 一致），若给 `S`
//! 加约束（`S: 提供校验配置`），所有自定义 state 的下游都会编译失败——为了一个
//! 上限参数破坏提取器的通用性不划算。
//!
//! # `fail_fast` 的真实语义
//!
//! `validator` 的 derive **没有短路求值能力**（一次 `validate()` 会把所有字段跑完），
//! 所以 `fail_fast = true` 只能把**输出**截断为 1 条，省不掉计算。这一点在
//! `docs/08` 里写明，避免运维以为它能省 CPU。

use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ValidationSection;

/// 单次返回错误数的**默认**上限（与 `[validation].max_errors` 默认值一致）。
pub const DEFAULT_MAX_ERRORS: usize = 10;

/// 单次返回错误数的**硬上限**。
///
/// 超过这个数的明细对前端没有价值，却要为每条错误付一次 `format!` + 哈希插入；
/// `max_errors` 配成天文数字时按本值截断（不报错——配置层已经拦过一道，
/// 这里防的是绕过配置校验直接调 [`set_global`] 的场景）。
pub const MAX_ERRORS_HARD_CAP: usize = 100;

/// 校验行为选项（`[validation]` 段在请求路径上的投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationOptions {
    /// 单次最多返回多少条错误。
    max_errors: usize,
    /// 是否只返回首条错误。
    fail_fast: bool,
}

impl Default for ValidationOptions {
    fn default() -> Self {
        Self {
            max_errors: DEFAULT_MAX_ERRORS,
            fail_fast: false,
        }
    }
}

impl ValidationOptions {
    /// 由显式上限与开关构造（越界值在读取时被归一到合法区间）。
    #[must_use]
    pub const fn new(max_errors: usize, fail_fast: bool) -> Self {
        Self {
            max_errors,
            fail_fast,
        }
    }

    /// 由配置段构造。
    #[must_use]
    pub fn from_section(section: &ValidationSection) -> Self {
        Self {
            max_errors: usize::try_from(section.max_errors).unwrap_or(MAX_ERRORS_HARD_CAP),
            fail_fast: section.fail_fast,
        }
    }

    /// 由显式上限构造（`fail_fast = false`）。
    ///
    /// 上限为 `0` 时归一到 1：返回 0 条明细等于「校验失败但不告诉客户端哪里错了」，
    /// 与 `10001` 的契约（带 `details`）矛盾。
    #[must_use]
    pub fn with_max_errors(max_errors: usize) -> Self {
        Self {
            max_errors,
            fail_fast: false,
        }
    }

    /// 单次最多返回多少条错误（已应用硬上限）。
    #[must_use]
    pub fn max_errors(&self) -> usize {
        self.max_errors.clamp(1, MAX_ERRORS_HARD_CAP)
    }

    /// 是否只返回首条错误。
    #[must_use]
    pub fn fail_fast(&self) -> bool {
        self.fail_fast
    }

    /// 实际生效的明细条数上限。
    ///
    /// `fail_fast` 优先：它为真时上限恒为 1。
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        if self.fail_fast { 1 } else { self.max_errors() }
    }
}

// —— 进程级单元格 ——
//
// 编码进单个 `AtomicU64`（bit0 = fail_fast，bit1.. = max_errors）：
// 读侧只有一次 `Relaxed` load，无分配、无锁。配置是幂等的（同一份快照写同样的值），
// 故不需要任何顺序保证——不存在「先看到上限、后看到 fail_fast」会出错的情况，
// 因为二者都只会整体被替换成某个合法快照的值。

/// `max_errors` 在单元格里的位移（bit0 留给 `fail_fast`）。
const MAX_ERRORS_SHIFT: u64 = 1;

/// 进程级选项单元格（初值 = [`ValidationOptions::default`]）。
static OPTIONS: AtomicU64 = AtomicU64::new((DEFAULT_MAX_ERRORS as u64) << MAX_ERRORS_SHIFT);

/// 读取当前进程的校验选项。
#[must_use]
pub fn global() -> ValidationOptions {
    decode(OPTIONS.load(Ordering::Relaxed))
}

/// 写入当前进程的校验选项。
///
/// 由 [`ConfigHandle::new`](crate::config::ConfigHandle::new) /
/// [`ConfigHandle::store`](crate::config::ConfigHandle::store) 调用，
/// 因此配置热更新会即时生效。
///
/// 手工装配 `Router`（不经过 `AppState`）且需要自定义上限时，可在启动期调用一次。
pub fn set_global(options: ValidationOptions) {
    OPTIONS.store(encode(options), Ordering::Relaxed);
}

const fn encode(options: ValidationOptions) -> u64 {
    ((options.max_errors as u64) << MAX_ERRORS_SHIFT) | (options.fail_fast as u64)
}

fn decode(raw: u64) -> ValidationOptions {
    ValidationOptions {
        max_errors: usize::try_from(raw >> MAX_ERRORS_SHIFT).unwrap_or(MAX_ERRORS_HARD_CAP),
        fail_fast: raw & 1 == 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_ten_errors_without_fail_fast() {
        let options = ValidationOptions::default();
        assert_eq!(options.max_errors(), 10);
        assert!(!options.fail_fast());
        assert_eq!(options.effective_limit(), 10);
    }

    #[test]
    fn should_map_section_to_options() {
        let section = ValidationSection {
            enabled: true,
            fail_fast: true,
            max_errors: 3,
        };
        let options = ValidationOptions::from_section(&section);
        assert_eq!(options.max_errors(), 3);
        assert!(options.fail_fast());
        // fail_fast 优先：只返回首条
        assert_eq!(options.effective_limit(), 1);
    }

    #[test]
    fn should_clamp_out_of_range_limits() {
        assert_eq!(ValidationOptions::with_max_errors(0).effective_limit(), 1);
        assert_eq!(
            ValidationOptions::with_max_errors(usize::MAX).effective_limit(),
            MAX_ERRORS_HARD_CAP
        );
    }

    #[test]
    fn should_roundtrip_through_atomic_cell() {
        // 编码 / 解码必须无损：单元格里存的就是「上限 + 开关」
        let cases = [
            ValidationOptions::default(),
            ValidationOptions::with_max_errors(1),
            ValidationOptions {
                max_errors: 77,
                fail_fast: true,
            },
        ];
        for options in cases {
            assert_eq!(decode(encode(options)), options);
        }
    }

    #[test]
    fn should_persist_global_options() {
        // 进程级单元格是全局的：本用例独占写入后再读回，不与其他用例共享断言
        let options = ValidationOptions {
            max_errors: 5,
            fail_fast: false,
        };
        set_global(options);
        assert_eq!(global(), options);
        // 复原，避免影响同进程内其他用例（默认值即「未配置」的语义）
        set_global(ValidationOptions::default());
        assert_eq!(global(), ValidationOptions::default());
    }
}
