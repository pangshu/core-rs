//! `[validation]` 配置段（feature `security`）。

use serde::Deserialize;

use crate::validation::{MAX_ERRORS_HARD_CAP, options::ValidationOptions};

/// 输入校验配置段。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ValidationSection {
    /// 校验总开关。
    ///
    /// **只允许 `true`**：把它做成运行时开关等于给全站输入校验留一个
    /// 「一行配置关掉」的 fail-open 入口（配置中心被改、环境变量被覆盖、
    /// 排障时随手关掉忘了开——任何一种都直接变成 0 校验）。因此
    /// `enabled = false` 会在**启动期拒绝启动**，而不是静默降级。
    ///
    /// 字段保留而非删除，是为了让「试图关掉校验」这个动作显式失败，
    /// 而不是被 `serde` 当未知字段静默忽略。
    pub enabled: bool,
    /// 是否只返回首条错误。
    ///
    /// 注意：`validator` 的 derive 没有短路求值能力（一次 `validate()`
    /// 会把所有字段跑完），本开关只截断**输出**，省不掉计算。
    pub fail_fast: bool,
    /// 单次最多返回的错误数（`1..=100`）。
    ///
    /// 上限直接决定错误明细的内存占用：列表型 DTO（`Vec<Nested>`）在
    /// 2 MiB 请求体下可产生十几万条明细，不截断就是现成的内存 / CPU 放大面。
    pub max_errors: u32,
}

impl Default for ValidationSection {
    fn default() -> Self {
        Self {
            enabled: true,
            fail_fast: false,
            // 与 `validation::DEFAULT_MAX_ERRORS` 一致（单测里有对齐断言防漂移）
            max_errors: 10,
        }
    }
}

impl ValidationSection {
    /// 启动期校验（fail-fast）。
    ///
    /// # Errors
    /// `enabled = false`（不允许关闭校验），或 `max_errors` 越界时返回错误文案。
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Err(
                "[validation].enabled = false 被拒绝：输入校验不允许关闭（关掉等于全站 0 校验）。\
                 如确实需要放行某些输入，请在对应字段上放宽规则，而不是关闭整层"
                    .to_string(),
            );
        }
        let max_errors = usize::try_from(self.max_errors).unwrap_or(usize::MAX);
        if max_errors == 0 || max_errors > MAX_ERRORS_HARD_CAP {
            return Err(format!(
                "[validation].max_errors 必须在 1..={MAX_ERRORS_HARD_CAP} 之间（当前 {}）：\
                 0 等于「失败但不告诉客户端哪里错了」，过大则明细体积与 CPU 开销失控",
                self.max_errors
            ));
        }
        Ok(())
    }

    /// 投影为请求路径上使用的选项。
    #[must_use]
    pub fn to_options(&self) -> ValidationOptions {
        ValidationOptions::from_section(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_enabled_non_fail_fast() {
        let s = ValidationSection::default();
        assert!(s.enabled);
        assert!(!s.fail_fast);
        assert_eq!(s.max_errors, 10);
        assert!(s.validate().is_ok(), "默认配置必须能通过启动校验");
        // 防漂移：配置默认值与 `validation` 模块的默认上限必须一致
        assert_eq!(
            usize::try_from(s.max_errors).unwrap(),
            crate::validation::DEFAULT_MAX_ERRORS
        );
    }

    #[test]
    fn should_parse_toggles() {
        let json = r#"{ "fail_fast": true, "max_errors": 3 }"#;
        let s: ValidationSection = serde_json::from_str(json).unwrap();
        assert!(s.fail_fast);
        assert_eq!(s.max_errors, 3);
        assert_eq!(s.to_options().effective_limit(), 1);
    }

    #[test]
    fn should_refuse_disabling_validation() {
        // 关掉校验 = 全站 0 校验，必须拒绝启动而不是静默降级
        let s = ValidationSection {
            enabled: false,
            ..ValidationSection::default()
        };
        let error = s.validate().expect_err("enabled = false 必须拒绝启动");
        assert!(
            error.contains("enabled"),
            "错误文案应指出是哪个开关：{error}"
        );
    }

    #[test]
    fn should_reject_out_of_range_max_errors() {
        for max_errors in [0, 101, u32::MAX] {
            let s = ValidationSection {
                max_errors,
                ..ValidationSection::default()
            };
            assert!(s.validate().is_err(), "max_errors = {max_errors} 应被拒绝");
        }
    }
}
