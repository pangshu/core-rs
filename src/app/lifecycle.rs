//! 启动 / 关闭生命周期钩子。
//!
//! [`AppState::build`](crate::app::AppState::build) 只负责**连接资源**；「资源就绪之后
//! 才做的初始化」（缓存预热、加载字典、向注册中心登记）与对称的清理动作，由应用注册到
//! [`LifecycleHooks`] 并在启动末 / 关闭前**显式**执行：
//!
//! - **启动**：[`LifecycleHooks::run_startup`]，按**注册顺序**执行；任一钩子失败即
//!   **逆序回滚**已完成的钩子并返回 [`LifecycleError`]（启动期失败要挡住进程，
//!   见 `docs/02` §13）。
//! - **关闭**：[`LifecycleHooks::run_shutdown`]，按**注册逆序**执行（后依赖先释放）；
//!   单个失败只记日志、**不中断**后续清理，最后汇总成 [`LifecycleReport`]。
//!
//! **顺序即契约**：`docs/02` §3 的关闭顺序（realtime → HTTP → queue → scheduler →
//! cache/db）落地方式就是「注册顺序 = 启动顺序」+ 关闭时逆序。
//!
//! ```no_run
//! use aurora::prelude::*;
//!
//! # async fn run(state: AppState) -> anyhow::Result<()> {
//! // 启动末执行（按注册顺序）
//! state.lifecycle.on_startup("cache-warmup", || async {
//!     Ok::<(), String>(())
//! });
//! // 关闭时执行（按注册逆序）
//! state.lifecycle.on_shutdown("flush-metrics", || async {
//!     Ok::<(), String>(())
//! });
//!
//! state.lifecycle.run_startup().await?;
//! // ... build_router / serve ...
//! state.lifecycle.run_shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! # 三条硬约束
//!
//! 1. **注册必须早于启动阶段**：`run_startup` 之后再注册会被**忽略**并记 warn——
//!    否则「关了却没启动过」的钩子会去清理不存在的资源。
//! 2. **启动失败不自动退出**：`run_startup` 只返回错误，是否终止进程由调用方决定
//!    （应用通常 `?` 到 `anyhow`）。
//! 3. **超时的钩子不回滚**：超时意味着副作用状态未知，盲目清理可能破坏正在进行的
//!    初始化；只有**明确成功**的钩子才进入回滚列表。

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;

use crate::utils::string::log_safe;

/// 默认单个钩子超时。
///
/// 钩子是**顺序**执行的，总耗时上限约为 `钩子数 × 超时`；预热类钩子确实需要更久时
/// 用 [`LifecycleHooks::set_timeout`] 调大。
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// 钩子返回的错误进日志时的最大字符数（先单行化 + 截断，防日志注入）。
const MAX_HOOK_LOG_CHARS: usize = 512;

const PHASE_IDLE: u8 = 0;
const PHASE_STARTED: u8 = 1;
const PHASE_CLOSED: u8 = 2;

/// 单个生命周期钩子。
///
/// 两个方法都有默认空实现：只做一侧的钩子只重写对应方法即可；两侧都要记录时
/// 同时重写 `on_startup` + `on_shutdown`（或直接用 [`LifecycleHooks::on_startup`] /
/// [`LifecycleHooks::on_shutdown`] 的闭包版）。
#[async_trait]
pub trait LifecycleHook: Send + Sync + 'static {
    /// 钩子名（用于日志与失败明细）。
    fn name(&self) -> &str;

    /// 启动阶段：在监听端口**之前**执行。
    ///
    /// `Err(msg)` 会中止启动并触发已完成钩子的逆序回滚；`msg` 只进日志/错误明细。
    async fn on_startup(&self) -> Result<(), String> {
        Ok(())
    }

    /// 关闭阶段：在 HTTP 服务**停止接收新请求之后**执行。
    ///
    /// 失败只记日志，不会阻断其余钩子的清理。
    async fn on_shutdown(&self) -> Result<(), String> {
        Ok(())
    }

    /// 是否参与关闭阶段。
    ///
    /// 只注册了启动侧的钩子（如 [`FnStartup`]）返回 `false`：关闭期直接跳过，
    /// 不调 `on_shutdown`、不记日志、不计入 [`LifecycleReport::ran`]——否则报告里
    /// 会冒出一堆「什么都没做」的条目。
    fn has_shutdown(&self) -> bool {
        true
    }
}

/// 单个钩子的失败明细。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFailure {
    /// 钩子名。
    pub hook: String,
    /// 失败原因（已单行化 + 截断）。
    pub error: String,
}

impl fmt::Display for HookFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`: {}", self.hook, self.error)
    }
}

/// 启动阶段失败。
///
/// 实现 [`std::error::Error`]，可直接经 `?` 汇入应用层 `anyhow`。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("lifecycle hook `{hook}` failed during startup: {error}")]
pub struct LifecycleError {
    /// 失败钩子名。
    pub hook: String,
    /// 失败原因（已单行化 + 截断）。
    pub error: String,
}

/// 关闭阶段汇总结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleReport {
    /// 执行成功的钩子名（按实际执行顺序，即注册逆序）。
    pub ran: Vec<String>,
    /// 失败的钩子（不影响其余钩子继续执行）。
    pub failures: Vec<HookFailure>,
}

impl LifecycleReport {
    /// 是否全部成功。
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// 内部状态：钩子表 + 单钩子超时。
struct Registry {
    hooks: Vec<Arc<dyn LifecycleHook>>,
    timeout: Duration,
}

struct Inner {
    registry: RwLock<Registry>,
    /// `PHASE_IDLE` → `PHASE_STARTED` → `PHASE_CLOSED`。
    phase: AtomicU8,
}

/// 生命周期钩子注册表。
///
/// 以 `Arc` 共享（随 [`AppState`](crate::app::AppState) 克隆），可在装配后继续
/// [`register`](Self::register)；注册只影响之后的启动 / 关闭阶段。
#[derive(Clone)]
pub struct LifecycleHooks {
    inner: Arc<Inner>,
}

impl LifecycleHooks {
    /// 以 [`DEFAULT_HOOK_TIMEOUT`] 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                registry: RwLock::new(Registry {
                    hooks: Vec::new(),
                    timeout: DEFAULT_HOOK_TIMEOUT,
                }),
                phase: AtomicU8::new(PHASE_IDLE),
            }),
        }
    }

    /// 设置单钩子超时（启动 / 关闭两侧共用）。
    pub fn set_timeout(&self, timeout: Duration) {
        self.write().timeout = timeout;
    }

    /// 当前单钩子超时。
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.read().timeout
    }

    /// 注册一个钩子（按调用顺序参与启动，启动阶段之后注册会被忽略）。
    pub fn register(&self, hook: impl LifecycleHook) {
        self.register_shared(Arc::new(hook));
    }

    /// 注册「仅启动」钩子（闭包版，便于应用层就地写预热逻辑）。
    pub fn on_startup<F, Fut>(&self, name: impl Into<String>, hook: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send,
    {
        self.register(FnStartup {
            name: name.into(),
            hook,
        });
    }

    /// 注册「仅关闭」钩子（闭包版）。
    pub fn on_shutdown<F, Fut>(&self, name: impl Into<String>, hook: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send,
    {
        self.register(FnShutdown {
            name: name.into(),
            hook,
        });
    }

    /// 已注册钩子数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().hooks.len()
    }

    /// 是否没有任何钩子。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 执行启动阶段（**注册顺序**，失败即逆序回滚已完成钩子）。
    ///
    /// 重复调用是空操作：启动阶段只跑一次。
    ///
    /// # Errors
    /// 任一钩子返回错误、panic 或超时时返回 [`LifecycleError`]；此时已完成钩子的
    /// `on_shutdown` 已经逆序执行过，注册表进入关闭态（后续 [`run_shutdown`]
    /// 不会重复清理）。
    ///
    /// [`run_shutdown`]: Self::run_shutdown
    pub async fn run_startup(&self) -> Result<(), LifecycleError> {
        if self
            .inner
            .phase
            .compare_exchange(
                PHASE_IDLE,
                PHASE_STARTED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            tracing::warn!("生命周期启动阶段已执行过，忽略重复调用");
            return Ok(());
        }

        let (hooks, timeout) = self.snapshot();
        let mut completed: Vec<Arc<dyn LifecycleHook>> = Vec::with_capacity(hooks.len());
        for hook in hooks {
            let name = hook_name(hook.as_ref());
            match run_one(Arc::clone(&hook), Stage::Startup, timeout).await {
                Ok(()) => {
                    tracing::debug!(hook = %name, "生命周期启动钩子已执行");
                    completed.push(hook);
                }
                Err(failure) => {
                    tracing::error!(
                        hook = %failure.hook,
                        error = %failure.error,
                        "生命周期启动钩子失败，开始逆序回滚已完成钩子"
                    );
                    rollback(&completed, timeout).await;
                    // 回滚已覆盖「明确成功」的钩子；再调 run_shutdown 只会重复清理。
                    self.inner.phase.store(PHASE_CLOSED, Ordering::SeqCst);
                    return Err(LifecycleError {
                        hook: failure.hook,
                        error: failure.error,
                    });
                }
            }
        }
        Ok(())
    }

    /// 执行关闭阶段（**注册逆序**，单个失败不中断后续清理）。
    ///
    /// 重复调用是空操作；未执行启动阶段就调用会记 warn 并按**尽力清理**执行——
    /// 关闭期丢资源比丢一条日志危险得多。
    ///
    /// 失败已在内部记日志（含汇总）；返回值只是给需要**结构化明细**的调用方
    /// （告警、退出码、测试断言）用的，可以直接丢弃。
    pub async fn run_shutdown(&self) -> LifecycleReport {
        let previous = self.inner.phase.swap(PHASE_CLOSED, Ordering::SeqCst);
        if previous == PHASE_CLOSED {
            tracing::warn!("生命周期关闭阶段已执行过，忽略重复调用");
            return LifecycleReport::default();
        }
        if previous == PHASE_IDLE {
            tracing::warn!("未执行启动阶段就进入关闭阶段：关闭钩子按尽力清理执行");
        }

        let (hooks, timeout) = self.snapshot();
        let mut report = LifecycleReport::default();
        for hook in hooks.iter().rev().filter(|hook| hook.has_shutdown()) {
            let name = hook_name(hook.as_ref());
            match run_one(Arc::clone(hook), Stage::Shutdown, timeout).await {
                Ok(()) => {
                    tracing::debug!(hook = %name, "生命周期关闭钩子已执行");
                    report.ran.push(name);
                }
                Err(failure) => {
                    tracing::warn!(
                        hook = %failure.hook,
                        error = %failure.error,
                        "生命周期关闭钩子失败，继续清理其余钩子"
                    );
                    report.failures.push(failure);
                }
            }
        }

        if report.is_ok() {
            tracing::info!(hooks = report.ran.len(), "生命周期关闭钩子全部执行完成");
        } else {
            tracing::warn!(
                hooks = report.ran.len(),
                failed = report.failures.len(),
                "生命周期关闭阶段结束（存在失败）"
            );
        }
        report
    }

    /// 注册共享句柄（[`register`](Self::register) 的落地点：先登记时序检查，再入表）。
    fn register_shared(&self, hook: Arc<dyn LifecycleHook>) {
        if self.inner.phase.load(Ordering::SeqCst) != PHASE_IDLE {
            tracing::warn!(
                hook = %hook_name(hook.as_ref()),
                "生命周期钩子注册晚于启动阶段，已忽略"
            );
            return;
        }
        self.write().hooks.push(hook);
    }

    /// 钩子表 + 超时的快照（锁中毒时恢复内部值，而不是当成空表）。
    fn snapshot(&self) -> (Vec<Arc<dyn LifecycleHook>>, Duration) {
        let registry = self.read();
        (registry.hooks.clone(), registry.timeout)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Registry> {
        self.inner
            .registry
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Registry> {
        self.inner
            .registry
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for LifecycleHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LifecycleHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LifecycleHooks")
            .field("hooks", &self.len())
            .field("timeout", &self.timeout())
            .finish_non_exhaustive()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 最后一道防呆：启动阶段跑过、却从未进入关闭阶段。异步清理无法在 Drop 里做，
        // 只能把「漏调 run_shutdown()」这件事喊出来。
        if self.phase.load(Ordering::SeqCst) == PHASE_STARTED {
            let is_empty = self
                .registry
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .hooks
                .is_empty();
            if !is_empty {
                tracing::warn!("AppState 已释放但未调用 run_shutdown()：生命周期关闭钩子没有执行");
            }
        }
    }
}

/// 钩子执行阶段（决定调用哪个方法）。
#[derive(Clone, Copy)]
enum Stage {
    Startup,
    Shutdown,
}

/// 执行单个钩子：独立超时 + panic 隔离 + 错误清洗。
async fn run_one(
    hook: Arc<dyn LifecycleHook>,
    stage: Stage,
    timeout: Duration,
) -> Result<(), HookFailure> {
    let name = hook_name(hook.as_ref());

    // spawn 而不是直接 await：钩子 panic 不该炸掉调用方——启动期还要回滚、
    // 关闭期还要把剩下的清理走完。
    let mut handle = tokio::spawn(async move {
        match stage {
            Stage::Startup => hook.on_startup().await,
            Stage::Shutdown => hook.on_shutdown().await,
        }
    });

    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(raw))) => Err(HookFailure {
            hook: name,
            error: log_safe(&raw, MAX_HOOK_LOG_CHARS),
        }),
        Ok(Err(join)) => {
            tracing::error!(hook = %name, error = %join, "生命周期钩子任务异常结束");
            Err(HookFailure {
                hook: name,
                error: if join.is_panic() {
                    "hook panicked".to_string()
                } else {
                    "hook task cancelled".to_string()
                },
            })
        }
        Err(_) => {
            // 超时必须 abort：丢掉 JoinHandle 只是「脱管」，挂住的钩子会在后台继续跑。
            handle.abort();
            Err(HookFailure {
                hook: name,
                error: format!("hook timed out after {timeout:?}"),
            })
        }
    }
}

/// 启动失败时**逆序**回滚已明确成功的钩子（失败只记日志）。
async fn rollback(completed: &[Arc<dyn LifecycleHook>], timeout: Duration) {
    for hook in completed.iter().rev().filter(|hook| hook.has_shutdown()) {
        let name = hook_name(hook.as_ref());
        match run_one(Arc::clone(hook), Stage::Shutdown, timeout).await {
            Ok(()) => tracing::debug!(hook = %name, "启动失败回滚：钩子已清理"),
            Err(failure) => tracing::warn!(
                hook = %failure.hook,
                error = %failure.error,
                "启动失败回滚：钩子清理失败"
            ),
        }
    }
}

fn hook_name(hook: &dyn LifecycleHook) -> String {
    log_safe(hook.name(), MAX_HOOK_LOG_CHARS)
}

/// 由闭包构造的「仅启动」钩子。
pub struct FnStartup<F> {
    name: String,
    hook: F,
}

impl<F> FnStartup<F> {
    /// 以钩子名与启动闭包构造。
    pub fn new(name: impl Into<String>, hook: F) -> Self {
        Self {
            name: name.into(),
            hook,
        }
    }
}

#[async_trait]
impl<F, Fut> LifecycleHook for FnStartup<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn on_startup(&self) -> Result<(), String> {
        (self.hook)().await
    }

    fn has_shutdown(&self) -> bool {
        false
    }
}

/// 由闭包构造的「仅关闭」钩子。
pub struct FnShutdown<F> {
    name: String,
    hook: F,
}

impl<F> FnShutdown<F> {
    /// 以钩子名与关闭闭包构造。
    pub fn new(name: impl Into<String>, hook: F) -> Self {
        Self {
            name: name.into(),
            hook,
        }
    }
}

#[async_trait]
impl<F, Fut> LifecycleHook for FnShutdown<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn on_shutdown(&self) -> Result<(), String> {
        (self.hook)().await
    }
}
