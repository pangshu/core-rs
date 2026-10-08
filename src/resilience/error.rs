//! 弹性调用错误 [`ResilienceError`]：区分被熔断拒绝与底层错误。

/// 弹性调用失败：区分被熔断拒绝与底层错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 并由 thiserror 实现 Error
pub enum ResilienceError<E> { // 弹性调用错误枚举
    #[error("circuit `{dep}` is open (半开探测中，稍后重试)")] // 该变体的 Display 文案
    Open { dep: String }, // 熔断打开被拒绝，携带依赖名
    #[error("retries exhausted after {attempts} attempts: {source}")] // 该变体的 Display 文案
    Exhausted { attempts: u32, source: E }, // 重试耗尽，携带尝试次数与底层错误
}

impl<E> ResilienceError<E> {
    pub fn into_inner(self) -> E { // 取出底层错误（仅 Exhausted 可用）
        match self {
            Self::Exhausted { source, .. } => source, // 重试耗尽：返回底层错误
            Self::Open { .. } => panic!("Open has no inner error"), // 熔断拒绝无底层错误，直接 panic
        }
    }

    /// 是否被熔断拒绝（调用方可据此走降级路径）
    pub fn is_open(&self) -> bool { // 判断是否为熔断拒绝
        matches!(self, Self::Open { .. }) // Open 变体返回 true
    }
}
