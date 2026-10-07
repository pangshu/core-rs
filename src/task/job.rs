//! Job 定义（文档 三·19）：名称、表达式、handler；超时与重试由 handler 内部
//! 自行处理（调度器只负责触发；长任务建议投递到 queue 执行）。

use std::future::Future; // 引入 Future trait，用于定义回调返回类型
use std::pin::Pin; // 引入 Pin，用于装箱后固定 Future 的内存位置
use std::sync::Arc; // 引入 Arc，让 handler 闭包可被多线程共享克隆

/// 定时任务回调返回的 Future
pub type JobFuture = Pin<Box<dyn Future<Output = ()> + Send>>; // 可发送的装箱 Future，输出为 ()

/// 任务回调：拿到 handler 自身闭包（多实例互斥由框架层包在闭包外）
pub type JobHandler = Arc<dyn Fn() -> JobFuture + Send + Sync>; // 可共享的零参闭包类型，每次调用产出新的 JobFuture

#[derive(Clone)] // 派生 Clone，使 Job 可被克隆后注册到调度器
pub struct Job { // 定时任务定义：名称 + 回调
    /// 任务名（与 `[task].jobs` 配置的 name 对应）
    pub name: String, // 任务名，用于与配置声明匹配
    f: JobHandler, // 私有回调，外部只能通过 handler() 取用
}

impl std::fmt::Debug for Job { // 手动实现 Debug，避免对闭包施加 Debug 约束
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 只打印 name 字段
        f.debug_struct("Job").field("name", &self.name).finish() // 输出形如 Job { name: "..." } 的调试串
    }
}

impl Job { // Job 的构造与访问方法
    pub fn new<F>(name: impl Into<String>, f: F) -> Self // 以名称与闭包构造任务
    where
        F: Fn() -> JobFuture + Send + Sync + 'static, // 闭包需可多次调用且线程安全
    {
        Self { // 组装 Job
            name: name.into(), // 把传入名称转为 String
            f: Arc::new(f), // 把闭包装箱进 Arc 以便共享
        }
    }

    /// 从 async 闭包快捷构造
    pub fn async_fn<F, Fut>(name: impl Into<String>, f: F) -> Self // 以 async 闭包构造任务（自动装箱）
    where
        F: Fn() -> Fut + Send + Sync + 'static, // 外层闭包每次产出新的 Future
        Fut: Future<Output = ()> + Send + 'static, // 产出的 Future 可发送且为 'static
    {
        Self::new(name, move || Box::pin(f())) // 用 move 闭包把 Future 装箱后委托给 new
    }

    pub(crate) fn handler(&self) -> JobHandler { // 取出回调的共享句柄（仅 crate 内可见）
        self.f.clone() // 克隆 Arc，引用计数 +1
    }
}
