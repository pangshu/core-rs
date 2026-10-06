//! 舱壁（并发隔离，文档 三·7）：限制对单一依赖的并发，防雪崩。
//! 满载时调用方立即拿到 [`BulkheadRejection`]（不排队，快速失败）。

use tokio::sync::Semaphore;

#[derive(Debug, thiserror::Error)]
#[error("bulkhead `{dep}` saturated ({max} concurrent)")]
pub struct BulkheadRejection {
    pub dep: String,
    pub max: usize,
}

pub struct Bulkhead {
    dep: String,
    max: usize,
    semaphore: std::sync::Arc<Semaphore>,
}

impl Bulkhead {
    pub fn new(dep: impl Into<String>, max_permits: usize) -> Self {
        let max = max_permits.max(1);
        Self {
            dep: dep.into(),
            max,
            semaphore: std::sync::Arc::new(Semaphore::new(max)),
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// 有名额则执行，无名额快速失败（不排队）
    pub async fn call<T, E, Fut>(&self, fut: Fut) -> Result<T, BulkheadCallError<E>>
    where
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        let Ok(permit) = self.semaphore.clone().try_acquire_owned() else {
            return Err(BulkheadCallError::Rejected(BulkheadRejection {
                dep: self.dep.clone(),
                max: self.max,
            }));
        };
        let result = fut.await;
        drop(permit);
        result.map_err(BulkheadCallError::Inner)
    }
}

/// 舱壁调用结果：`Rejected`（被隔离拒绝）或 `Inner`（依赖自身错误）
#[derive(Debug, thiserror::Error)]
pub enum BulkheadCallError<E> {
    #[error(transparent)]
    Rejected(#[from] BulkheadRejection),
    #[error("dependency error: {0}")]
    Inner(E),
}
