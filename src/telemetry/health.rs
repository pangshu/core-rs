//! 健康检查探针。
//!
//! - `/health`：liveness，**仅探测进程存活，绝不访问 DB**。
//! - `/ready`：readiness，执行 [`HealthRegistry`] 中注册的探针（db / cache / queue /
//!   自定义），任一失败即返回 `503` + `50005`。
//!
//! 每个探针设**独立超时**（见 [`HealthRegistry::with_timeout`]）并**并发执行**：
//! 一次就绪判定耗时约等于最慢的那个探针，而不是各超时之和。
//!
//! `/ready` 对**未认证调用方**可见（`auth` 中间件对无 `Authorization` 的请求放行），
//! 因此这里有两道不能省的护栏：
//!
//! 1. **错误原文只进日志**：响应体里永远是 `{name} unavailable`。清洗放在
//!    [`HealthRegistry::run`] 边界，而不是指望每个探针作者记得调 [`probe_failure`]
//!    ——后者是「可选工具」，前者才是「保证」。
//! 2. **单飞 + 短 TTL 缓存**（[`HealthRegistry::with_cache_ttl`]）：否则一个廉价
//!    的匿名请求就能换来 N 次 DB/Redis/MQ 往返，`/ready` 会被当成放大器打依赖。

use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;
use tokio::sync::Mutex as AsyncMutex;

use crate::app::state::AppState;
use crate::errors::ErrorCode;
use crate::http::response::ApiResponse;
use crate::utils::string::log_safe;

/// 默认单探针超时。
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// 默认就绪结果缓存时长。
///
/// 取 1s：既能让同一轮并发探测只打一次依赖，又不会让「刚挂」的依赖被掩盖太久。
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(1);

/// 探针错误进日志时的最大字符数。
const MAX_PROBE_LOG_CHARS: usize = 512;

/// 探针失败的标准处理：错误原文只进日志，对外只返回固定文案。
///
/// 保留是为了兼容既有探针实现；**注册表已在 [`HealthRegistry::run`] 里统一做同样的
/// 清洗**，因此新代码直接 `Err(error.to_string())` 也不会泄漏——调用本函数只会让
/// 同一次失败多记一条日志。
///
/// 日志侧做了**单行化 + 截断**：错误文本来自对端服务（数据库 / Redis），其中的换行
/// 能把一条日志伪造成多行（日志注入）。
pub fn probe_failure(probe: &str, error: impl fmt::Display) -> String {
    let detail = log_safe(&error.to_string(), MAX_PROBE_LOG_CHARS);
    tracing::warn!(probe, error = %detail, "health probe failed");
    public_failure(probe)
}

/// 对外的固定失败文案（不含任何内部细节）。
fn public_failure(probe: &str) -> String {
    format!("{probe} unavailable")
}

/// 单个依赖探针。
#[async_trait]
pub trait HealthCheck: Send + Sync + 'static {
    /// 探针名（用于就绪输出与日志）。
    fn name(&self) -> &str;

    /// 执行探测：`Ok(())` 表示健康；`Err(msg)` 携带失败原因。
    ///
    /// `msg` 可以写内部细节（连接串、库名、驱动报错）：它**只会进日志**，
    /// 对外一律替换成 `{name} unavailable`。
    async fn check(&self) -> Result<(), String>;
}

/// 单个探针的执行结果。
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    /// 探针名。
    pub name: String,
    /// 是否健康。
    pub healthy: bool,
    /// 对外文案（健康时为 `None`；失败时是固定文案，**不含内部细节**）。
    pub message: Option<String>,
}

/// 就绪探测汇总结果。
#[derive(Debug, Clone, Serialize)]
pub struct HealthStatus {
    /// 全部探针是否健康。
    pub ready: bool,
    /// 各探针结果（按注册顺序）。
    pub checks: Vec<CheckOutcome>,
}

/// 探针注册表。
///
/// 以 `Arc` 共享，可在 `AppState::build` 之后继续 [`register`](Self::register)
/// ——注册结果对已挂载的 `/ready` 立即生效（并让缓存失效）。
#[derive(Clone)]
pub struct HealthRegistry {
    checks: Arc<RwLock<Vec<Arc<dyn HealthCheck>>>>,
    probe_timeout: Duration,
    cache_ttl: Duration,
    /// 上一次探测结果 + 时间戳；同时充当**单飞**闸门（`AsyncMutex` 持有到探测结束）。
    cache: Arc<AsyncMutex<Option<(Instant, HealthStatus)>>>,
}

impl HealthRegistry {
    /// 以默认探针超时 / 默认缓存时长创建注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_PROBE_TIMEOUT)
    }

    /// 以指定单探针超时创建注册表。
    #[must_use]
    pub fn with_timeout(probe_timeout: Duration) -> Self {
        Self {
            checks: Arc::new(RwLock::new(Vec::new())),
            probe_timeout,
            cache_ttl: DEFAULT_CACHE_TTL,
            cache: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// 设置就绪结果缓存时长。
    ///
    /// `Duration::ZERO` 关闭缓存（每次调用都真探测），但**仍保留单飞**：同一时刻的
    /// 并发调用只会有一轮探测落地。探测周期很短的部署可以调小或置零。
    #[must_use]
    pub fn with_cache_ttl(mut self, cache_ttl: Duration) -> Self {
        self.cache_ttl = cache_ttl;
        self
    }

    /// 注册一个探针。
    pub fn register(&self, check: impl HealthCheck) {
        let mut checks = match self.checks.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // 中毒 = 之前有人在持锁时 panic。绝不能把它当成「空表」，
                // 否则探针静默消失、`/ready` 恒 200（fail-open）。
                tracing::error!("健康探针注册表锁中毒，已恢复内部数据后继续注册");
                poisoned.into_inner()
            }
        };
        checks.push(Arc::new(check));
        drop(checks);

        // 新探针必须立刻参与就绪判定；拿不到缓存锁就等 TTL 自然过期（≤ 1s）。
        if let Ok(mut cache) = self.cache.try_lock() {
            *cache = None;
        }
    }

    /// 已注册探针数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// 是否没有注册任何探针。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 执行就绪探测并汇总结果（带单飞 + 短 TTL 缓存）。
    ///
    /// 缓存命中直接返回上一次结果；未命中时**持有锁**完成探测，因此并发调用者会
    /// 排队等待同一次探测，而不是各自再打一轮依赖。
    pub async fn run(&self) -> HealthStatus {
        let mut cache = self.cache.lock().await;
        if self.cache_ttl > Duration::ZERO
            && let Some((at, status)) = cache.as_ref()
            && at.elapsed() < self.cache_ttl
        {
            return status.clone();
        }

        let status = self.probe_all().await;
        *cache = Some((Instant::now(), status.clone()));
        status
    }

    /// 并发执行全部探针（每个探针独立超时），结果按注册顺序排列。
    async fn probe_all(&self) -> HealthStatus {
        let checks = self.snapshot();
        if checks.is_empty() {
            return HealthStatus {
                ready: true,
                checks: Vec::new(),
            };
        }

        let timeout = self.probe_timeout;
        let mut tasks = tokio::task::JoinSet::new();
        for (index, check) in checks.into_iter().enumerate() {
            // `check` 是 `Arc<dyn HealthCheck>`，move 进任务后 future 即 `'static`
            tasks.spawn(async move { (index, run_one(check.as_ref(), timeout).await) });
        }

        let mut outcomes: Vec<(usize, CheckOutcome)> = Vec::with_capacity(tasks.len());
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(pair) => outcomes.push(pair),
                Err(error) => {
                    // 探针自己 panic 了：判不健康，但别把整次判定拖垮
                    tracing::error!(%error, "健康探针任务异常结束");
                    outcomes.push((
                        usize::MAX,
                        CheckOutcome {
                            name: "probe".to_string(),
                            healthy: false,
                            message: Some(public_failure("probe")),
                        },
                    ));
                }
            }
        }
        // 并发完成顺序不确定；按注册顺序输出，响应体才稳定可 diff
        outcomes.sort_by_key(|(index, _)| *index);

        let outcomes: Vec<CheckOutcome> =
            outcomes.into_iter().map(|(_, outcome)| outcome).collect();
        let ready = outcomes.iter().all(|outcome| outcome.healthy);
        HealthStatus {
            ready,
            checks: outcomes,
        }
    }

    /// 探针列表快照（锁中毒时恢复内部值，而不是当作空表）。
    fn snapshot(&self) -> Vec<Arc<dyn HealthCheck>> {
        self.checks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// 执行单个探针：独立超时 + 错误清洗。
async fn run_one(check: &dyn HealthCheck, timeout: Duration) -> CheckOutcome {
    let name = check.name().to_string();
    match tokio::time::timeout(timeout, check.check()).await {
        Ok(Ok(())) => CheckOutcome {
            name,
            healthy: true,
            message: None,
        },
        Ok(Err(raw)) => {
            // 原文（可能含连接串 / 用户名 / 库名）只进日志，且先单行化 + 截断
            let detail = log_safe(&raw, MAX_PROBE_LOG_CHARS);
            tracing::warn!(probe = %name, error = %detail, "health probe failed");
            let message = public_failure(&name);
            CheckOutcome {
                name,
                healthy: false,
                message: Some(message),
            }
        }
        Err(_) => CheckOutcome {
            name,
            healthy: false,
            message: Some("probe timed out".to_string()),
        },
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HealthRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HealthRegistry")
            .field("checks", &self.len())
            .field("probe_timeout", &self.probe_timeout)
            .field("cache_ttl", &self.cache_ttl)
            .finish()
    }
}

/// 由闭包构造的探针（便于测试与轻量自定义）。
pub struct FnHealth<F> {
    name: String,
    probe: F,
}

impl<F> FnHealth<F> {
    /// 以探针名与探测闭包构造。
    pub fn new(name: impl Into<String>, probe: F) -> Self {
        Self {
            name: name.into(),
            probe,
        }
    }
}

#[async_trait]
impl<F, Fut> HealthCheck for FnHealth<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn check(&self) -> Result<(), String> {
        (self.probe)().await
    }
}

/// 构建健康检查路由片段。
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(liveness))
        .route("/ready", get(readiness))
}

/// liveness：进程存活即为 200。
async fn liveness() -> ApiResponse<&'static str> {
    ApiResponse::ok("ok")
}

/// readiness：执行已注册探针；全部健康返回 200，否则 503（`50005`）。
async fn readiness(State(state): State<AppState>) -> ApiResponse<HealthStatus> {
    let status = state.health.run().await;
    if status.ready {
        ApiResponse::with_data(ErrorCode::OK, "ok", status)
    } else {
        ApiResponse::with_data(ErrorCode::SERVICE_UNAVAILABLE, "service not ready", status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn should_be_ready_when_no_probes() {
        let registry = HealthRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.run().await.ready);
    }

    #[tokio::test]
    async fn should_report_not_ready_when_probe_fails() {
        let registry = HealthRegistry::new();
        registry.register(FnHealth::new("db", || async { Err("down".to_string()) }));
        let status = registry.run().await;
        assert!(!status.ready);
        assert_eq!(status.checks.len(), 1);
        // 对外只给固定文案
        assert_eq!(status.checks[0].message.as_deref(), Some("db unavailable"));
    }

    #[tokio::test]
    async fn should_treat_timeout_as_failure() {
        let registry = HealthRegistry::with_timeout(Duration::from_millis(10));
        registry.register(FnHealth::new("slow", || async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        }));
        let status = registry.run().await;
        assert!(!status.ready);
        assert_eq!(status.checks[0].message.as_deref(), Some("probe timed out"));
    }

    #[test]
    fn should_not_leak_probe_error_into_response_message() {
        // /ready 对未认证调用方可见：连接串、用户名、库名都不能进响应体
        let message = probe_failure(
            "database",
            "FATAL: password authentication failed for user \"aurora\" on db.internal:5432",
        );
        assert_eq!(message, "database unavailable");
        assert!(!message.contains("aurora"));
        assert!(!message.contains("db.internal"));
    }

    #[tokio::test]
    async fn should_sanitize_raw_probe_error_at_registry_boundary() {
        // 探针作者直接 `Err(e.to_string())` 也不能泄漏——清洗是注册表的责任
        let registry = HealthRegistry::new();
        registry.register(FnHealth::new("database", || async {
            Err("postgres://aurora:s3cr3t@db.internal:5432/app".to_string())
        }));
        let status = registry.run().await;
        let message = status.checks[0].message.as_deref().unwrap();
        assert_eq!(message, "database unavailable");
        assert!(!message.contains("s3cr3t"), "不得回显凭据：{message}");
        assert!(
            !message.contains("db.internal"),
            "不得回显内网地址：{message}"
        );
    }

    #[test]
    fn should_flatten_control_chars_in_probe_log() {
        // 对端返回的文本可能带换行，直接落盘等于让人伪造日志行
        let message = probe_failure("cache", "boom\n2026-01-01 ERROR forged admin login");
        assert_eq!(message, "cache unavailable");
    }

    #[tokio::test]
    async fn should_run_probes_concurrently() {
        // 3 个探针各睡 100ms：串行需要 300ms，并发应接近 100ms
        let registry = HealthRegistry::with_timeout(Duration::from_secs(5));
        for name in ["a", "b", "c"] {
            registry.register(FnHealth::new(name, || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(())
            }));
        }
        let started = Instant::now();
        let status = registry.run().await;
        let elapsed = started.elapsed();
        assert!(status.ready);
        assert!(
            elapsed < Duration::from_millis(250),
            "探针应并发执行，实际耗时 {elapsed:?}"
        );
        // 输出顺序 = 注册顺序
        let names: Vec<&str> = status.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn should_cache_result_within_ttl() {
        // 单飞 + TTL：多次调用只应真正探测一次
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = HealthRegistry::with_timeout(Duration::from_secs(1));
        let counter = Arc::clone(&calls);
        registry.register(FnHealth::new("db", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }));

        registry.run().await;
        registry.run().await;
        registry.run().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "TTL 内不应重复探测");

        // 注册新探针要立刻生效（缓存失效）
        registry.register(FnHealth::new("cache", || async { Ok(()) }));
        let status = registry.run().await;
        assert_eq!(status.checks.len(), 2);
    }

    #[tokio::test]
    async fn should_collapse_concurrent_probes_into_one_round() {
        // 单飞：N 个并发 `/ready` 只应产生一轮真实依赖往返（放大攻击的核心防线）
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = HealthRegistry::with_timeout(Duration::from_secs(1));
        let counter = Arc::clone(&calls);
        registry.register(FnHealth::new("db", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(())
            }
        }));

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let registry = registry.clone();
            tasks.spawn(async move { registry.run().await.ready });
        }
        while let Some(joined) = tasks.join_next().await {
            assert!(joined.unwrap());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "并发请求应合并成一轮探测");
    }

    #[tokio::test]
    async fn should_disable_cache_when_ttl_is_zero() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry =
            HealthRegistry::with_timeout(Duration::from_secs(1)).with_cache_ttl(Duration::ZERO);
        let counter = Arc::clone(&calls);
        registry.register(FnHealth::new("db", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }));

        registry.run().await;
        registry.run().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
