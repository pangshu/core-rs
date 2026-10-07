//! 舱壁（并发隔离，文档 三·7）：限制对单一依赖的并发，防雪崩。
//! 满载时调用方立即拿到 [`BulkheadRejection`]（不排队，快速失败）。

use tokio::sync::Semaphore; // 引入信号量，用作并发名额控制

#[derive(Debug, thiserror::Error)] // 派生 Debug 并由 thiserror 实现 Error
#[error("bulkhead `{dep}` saturated ({max} concurrent)")] // 该错误的 Display 文案
pub struct BulkheadRejection { // 舱壁满载时的拒绝错误
    pub dep: String, // 被隔离的依赖名
    pub max: usize, // 该舱壁的并发上限
}

pub struct Bulkhead { // 舱壁：对单一依赖的并发隔离
    dep: String, // 依赖名
    max: usize, // 并发上限
    semaphore: std::sync::Arc<Semaphore>, // 承载并发名额的信号量
}

impl Bulkhead {
    pub fn new(dep: impl Into<String>, max_permits: usize) -> Self { // 按依赖名与并发上限创建舱壁
        let max = max_permits.max(1); // 上限至少为 1，避免零名额
        Self {
            dep: dep.into(), // 转为 String 保存依赖名
            max, // 保存并发上限
            semaphore: std::sync::Arc::new(Semaphore::new(max)), // 初始化信号量
        }
    }

    pub fn max(&self) -> usize { // 返回并发上限
        self.max // 直接返回字段
    }

    /// 有名额则执行，无名额快速失败（不排队）
    pub async fn call<T, E, Fut>(&self, fut: Fut) -> Result<T, BulkheadCallError<E>> // 在并发名额内执行 Future
    where
        Fut: std::future::Future<Output = Result<T, E>>, // 待执行的业务 Future
    {
        let Ok(permit) = self.semaphore.clone().try_acquire_owned() else { // 尝试非阻塞获取一个名额
            return Err(BulkheadCallError::Rejected(BulkheadRejection { // 无名额：立即拒绝
                dep: self.dep.clone(), // 记录依赖名
                max: self.max, // 记录并发上限
            }));
        };
        let result = fut.await; // 有名额：执行真正的调用
        drop(permit); // 执行完立即归还名额
        result.map_err(BulkheadCallError::Inner) // 把依赖自身错误包装为 Inner 返回
    }
}

/// 舱壁调用结果：`Rejected`（被隔离拒绝）或 `Inner`（依赖自身错误）
#[derive(Debug, thiserror::Error)] // 派生 Debug 并由 thiserror 实现 Error
pub enum BulkheadCallError<E> { // 舱壁调用错误枚举
    #[error(transparent)] // 直接透传内层错误的 Display
    Rejected(#[from] BulkheadRejection), // 被舱壁拒绝（可由 BulkheadRejection 自动转换）
    #[error("dependency error: {0}")] // 该变体的 Display 文案
    Inner(E), // 依赖自身返回的错误
}
