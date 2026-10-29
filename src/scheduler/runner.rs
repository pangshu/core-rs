//! 调度器运行时。
//!
//! - **cron 任务**：交给 `tokio-cron-scheduler`（`Job::new_async_tz`）。
//! - **一次性任务**：自己 `tokio::spawn` + `sleep`——底层库的 one-shot 在
//!   「先 add 后 start」时序下不触发，故不依赖它；关闭时可直接取消。
//!
//! 每次触发都要过三道闸（见 `docs/03` §4.13）：
//!
//! 1. **进程内互斥**：同名任务上一次没跑完就跳过——`distributed` 关闭时同样生效。
//!    跨实例选主**不能**替代它：单实例下 cron 每 tick 都会新建一个执行 future。
//! 2. **跨实例选主**：`distributed.enabled` 时抢 `sched:{job}` 锁；执行期间由看门狗按
//!    `lock_ttl / 3` 续期，续期失败到「超过 TTL」即判定失锁并**取消任务**（宁可中断，
//!    也不能与其他实例双跑）。
//! 3. **执行超时**：`job_timeout` 到点取消，避免卡死任务长期占锁、并让每 tick 再叠一个
//!    永不结束的 future（最终耗尽连接池）。
//!
//! 任务**仅内存调度**：重启后按注册重新调度，停机期间错过的触发**不补偿**。
//! 关闭时先停新触发 → 等 `drain_timeout` 让在途任务收敛 → 再强制 abort 剩余。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono_tz::Tz;
use tokio::task::{JoinError, JoinHandle};
use tokio_cron_scheduler::{Job as CronJob, JobScheduler};

use crate::cache::{CacheService, DistributedLock};
use crate::config::SchedulerSection;
use crate::config::sections::scheduler::LockErrorPolicy;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::scheduler::distributed::LeaderElection;
use crate::scheduler::job::{FnJob, Job, JobFuture, JobHandler, JobSpec};

/// 任务名的字节上限（锁键 = `sched:` + name，`cache` 键总长上限 200B）。
const MAX_JOB_NAME_BYTES: usize = 128;

/// 执行计数（可读快照见 [`Scheduler::stats`]）。
///
/// 只增不减的 `AtomicU64`：应用可把它接到自己的指标系统或 `/metrics`。
#[derive(Debug, Default)]
pub struct ExecStats {
    /// 实际执行次数（含失败）。
    pub runs: AtomicU64,
    /// 因同名任务仍在执行而跳过的次数。
    pub skipped_overlap: AtomicU64,
    /// 因其他实例持有选主锁而跳过的次数。
    pub skipped_not_leader: AtomicU64,
    /// 选主锁获取失败的次数。
    pub lock_errors: AtomicU64,
    /// 执行超时次数。
    pub timeouts: AtomicU64,
    /// 执行期间失去选主锁（被判定失锁后取消）的次数。
    pub leader_lost: AtomicU64,
    /// 任务 panic 次数。
    pub panics: AtomicU64,
    /// 任务返回错误的次数。
    pub failures: AtomicU64,
    /// 因关闭而放弃的触发次数。
    pub skipped_draining: AtomicU64,
}

impl ExecStats {
    /// 生成一份可读快照（`(名称, 值)`，顺序固定）。
    #[must_use]
    pub fn snapshot(&self) -> [(&'static str, u64); 9] {
        let read = |value: &AtomicU64| value.load(Ordering::Relaxed);
        [
            ("runs", read(&self.runs)),
            ("skipped_overlap", read(&self.skipped_overlap)),
            ("skipped_not_leader", read(&self.skipped_not_leader)),
            ("lock_errors", read(&self.lock_errors)),
            ("timeouts", read(&self.timeouts)),
            ("leader_lost", read(&self.leader_lost)),
            ("panics", read(&self.panics)),
            ("failures", read(&self.failures)),
            ("skipped_draining", read(&self.skipped_draining)),
        ]
    }
}

/// 调度器（`AppState.scheduler`）。
///
/// `clone()` 共享同一份注册表与运行时。
#[derive(Clone)]
pub struct Scheduler {
    config: SchedulerSection,
    timezone: Tz,
    /// 分布式选主（`[distributed].enabled = true` 时存在）。
    election: Option<LeaderElection>,
    registrations: Arc<Mutex<Vec<Registration>>>,
    /// cron 任务运行时（仅当存在 cron 任务时创建）。
    inner: Arc<Mutex<Option<JobScheduler>>>,
    /// 一次性任务的句柄（关闭时取消）。
    oneshots: Arc<Mutex<Vec<JoinHandle<()>>>>,
    started: Arc<AtomicBool>,
    seq: Arc<AtomicU64>,
    /// 进程内在执行的任务名（同名互斥 + 关闭时 drain 依据）。
    inflight: Arc<Inflight>,
    /// 执行计数。
    stats: Arc<ExecStats>,
    /// 关闭中标记：置位后新的触发一律跳过。
    draining: Arc<AtomicBool>,
    /// 关闭信号：唤醒还在 `sleep` 的一次性任务，让它们立刻放弃。
    drain_signal: Arc<tokio::sync::Notify>,
    /// 重叠跳过日志的节流器（避免每秒一条 warn 刷爆日志）。
    overlap_warn: Arc<LogThrottle>,
}

struct Registration {
    job: Arc<dyn Job>,
    spec: JobSpec,
    /// 一次性任务是否已经 spawn 过（`shutdown` 后再 `start` 不重跑）。
    consumed: bool,
}

/// 进程内在执行的任务名集合。
#[derive(Default)]
struct Inflight {
    jobs: Mutex<HashSet<String>>,
    idle: tokio::sync::Notify,
}

impl Inflight {
    fn try_enter(self: &Arc<Self>, name: &str) -> Option<InflightGuard> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|error| error.into_inner());
        if !jobs.insert(name.to_string()) {
            return None;
        }
        Some(InflightGuard {
            inflight: Arc::clone(self),
            name: name.to_string(),
        })
    }

    fn is_empty(&self) -> bool {
        self.jobs
            .lock()
            .map(|jobs| jobs.is_empty())
            .unwrap_or(false)
    }

    fn len(&self) -> usize {
        self.jobs.lock().map(|jobs| jobs.len()).unwrap_or(0)
    }
}

/// 在途名额；**drop 即释放**（panic / 取消都会归还）。
struct InflightGuard {
    inflight: Arc<Inflight>,
    name: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let empty = {
            let mut jobs = self
                .inflight
                .jobs
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            jobs.remove(&self.name);
            jobs.is_empty()
        };
        if empty {
            // `notify_one`（而非 `notify_waiters`）：没有等待者时会留下一个许可，
            // 关掉「刚变空 → 还没注册 waiter」这段竞态窗口
            self.inflight.idle.notify_one();
        }
    }
}

/// 日志节流器：60 秒内最多放行一条（其余降级 debug），避免每秒一条 warn 刷爆日志。
#[derive(Default)]
struct LogThrottle {
    last_secs: AtomicU64,
}

impl LogThrottle {
    fn allow(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let last = self.last_secs.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 60 {
            return false;
        }
        self.last_secs
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

/// 单次执行所需的外部依赖快照（`clone` 廉价）。
#[derive(Clone)]
struct ExecContext {
    election: Option<LeaderElection>,
    inflight: Arc<Inflight>,
    stats: Arc<ExecStats>,
    job_timeout: Option<Duration>,
    lock_ttl: Duration,
    on_lock_error: LockErrorPolicy,
    draining: Arc<AtomicBool>,
    overlap_warn: Arc<LogThrottle>,
}

/// 一次执行的结局（用于计数与日志）。
enum Outcome {
    Completed(AppResult<()>),
    Timeout,
    LeaderLost,
    Panicked(String),
    Cancelled,
}

/// 任务体句柄：外层 future 被丢弃时**连带取消**内部任务，
/// 否则 `select!` 超时只会 detach 掉一个仍在运行的任务。
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("enabled", &self.config.enabled)
            .field("timezone", &self.config.timezone)
            .field("distributed", &self.election.is_some())
            .field("started", &self.started.load(Ordering::Relaxed))
            .field("draining", &self.draining.load(Ordering::Relaxed))
            .field("jobs", &self.job_names())
            .finish_non_exhaustive()
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        // 最后一个副本被丢弃且仍处于「已启动」：说明应用漏了 `shutdown()`
        if self.started.load(Ordering::Acquire) && Arc::strong_count(&self.started) == 1 {
            tracing::warn!(
                "调度器未调用 shutdown() 即被丢弃：在途任务不会被 drain，选主锁会残留到 TTL 过期"
            );
        }
    }
}

impl Scheduler {
    /// 按 `[scheduler]` 配置装配（`cache` 用于分布式选主）。
    ///
    /// # Errors
    /// 时区非法时返回 `50005`（启动期暴露）。
    pub fn from_config(config: &SchedulerSection, cache: CacheService) -> AppResult<Self> {
        let timezone = config.timezone.parse::<Tz>().map_err(|_| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!(
                    "scheduler.timezone = \"{}\" 不是合法的 IANA 时区",
                    config.timezone
                ),
            )
        })?;

        let election = config
            .distributed
            .enabled
            .then(|| LeaderElection::new(cache, config.distributed.lock_ttl));

        Ok(Self {
            config: config.clone(),
            timezone,
            election,
            registrations: Arc::new(Mutex::new(Vec::new())),
            inner: Arc::new(Mutex::new(None)),
            oneshots: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(AtomicBool::new(false)),
            seq: Arc::new(AtomicU64::new(0)),
            inflight: Arc::new(Inflight::default()),
            stats: Arc::new(ExecStats::default()),
            draining: Arc::new(AtomicBool::new(false)),
            drain_signal: Arc::new(tokio::sync::Notify::new()),
            overlap_warn: Arc::new(LogThrottle::default()),
        })
    }

    /// 是否启用（`[scheduler].enabled`）。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// 是否已启动。
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// 执行计数（可直接接到指标系统）。
    #[must_use]
    pub fn stats(&self) -> &ExecStats {
        &self.stats
    }

    /// 已注册任务名（按注册顺序）。
    #[must_use]
    pub fn job_names(&self) -> Vec<String> {
        self.registrations
            .lock()
            .map(|registrations| {
                registrations
                    .iter()
                    .map(|registration| registration.job.name().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 注册 cron 任务（**6 段，含秒**：`秒 分 时 日 月 周`）。
    ///
    /// 任务名固定为 `cron:{expr}`——它是**跨实例稳定的选主锁键**，因此同一表达式
    /// 只能注册一次；需要同一时刻跑多个任务时改用
    /// [`register_named`](Self::register_named) 显式命名。
    ///
    /// # Errors
    /// cron 表达式非法、任务名重复、或调度已启动时返回 `10001`。
    pub fn register_cron<F, Fut>(&self, expr: &str, handler: F) -> AppResult<&Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
    {
        validate_cron(expr, self.timezone)?;
        self.register_named(
            &format!("cron:{expr}"),
            JobSpec::Cron(expr.to_string()),
            handler,
        )
    }

    /// 注册一次性延迟任务（[`start`](Self::start) 后延迟 `delay` 触发一次）。
    ///
    /// 若在触发前应用关闭，该任务会被取消（与「错过的触发不补偿」一致）。
    ///
    /// # Errors
    /// 任务名重复或调度已启动时返回 `10001`。
    pub fn register_once<F, Fut>(&self, delay: Duration, handler: F) -> AppResult<&Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
    {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.push(
            FnJob::new(format!("once:{seq}"), boxed(handler)),
            JobSpec::Once(delay),
        )
    }

    /// 注册**显式命名**的任务（闭包形态）。
    ///
    /// 名字会进入选主锁键（`sched:{name}`），必须：
    /// 在应用内唯一、跨实例稳定（同一次部署的所有实例用同一个名字）、
    /// 不含控制字符、不超过 128 字节。**不要**用租户/用户输入拼接名字。
    ///
    /// # Errors
    /// 名字非法、cron 表达式非法、名字重复、或调度已启动时返回 `10001`。
    pub fn register_named<F, Fut>(&self, name: &str, spec: JobSpec, handler: F) -> AppResult<&Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
    {
        validate_job_name(name)?;
        if let JobSpec::Cron(expr) = &spec {
            validate_cron(expr, self.timezone)?;
        }
        self.push(FnJob::new(name, boxed(handler)), spec)
    }

    /// 注册自定义 [`Job`]（可自定义任务名与执行逻辑）。
    ///
    /// # Errors
    /// 任务名非法/重复、cron 表达式非法、或调度已启动时返回 `10001`。
    pub fn register<J: Job>(&self, job: J, spec: JobSpec) -> AppResult<&Self> {
        validate_job_name(job.name())?;
        if let JobSpec::Cron(expr) = &spec {
            validate_cron(expr, self.timezone)?;
        }
        self.push(job, spec)
    }

    /// 启动调度（重复调用无副作用；并发调用只有一个生效）。
    ///
    /// # Errors
    /// cron 调度器创建/启动失败时返回 `50005`；注册表损坏时返回 `50001`。
    pub async fn start(&self) -> AppResult<()> {
        if !self.config.enabled {
            tracing::info!("[scheduler].enabled = false，跳过调度启动");
            return Ok(());
        }
        // 先原子占位：并发 start 只允许一个通过（否则会建出两个 JobScheduler，
        // 后者覆盖前者、前者变成无法关闭的幽灵调度器）
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        self.draining.store(false, Ordering::Release);

        match self.start_inner().await {
            Ok(()) => {
                tracing::info!(
                    jobs = self.job_names().len(),
                    distributed = self.election.is_some(),
                    timezone = %self.config.timezone,
                    "调度器已启动"
                );
                Ok(())
            }
            Err(error) => {
                // 启动失败：回收已 spawn 的一次性任务，避免留下无人管理的后台任务
                if let Ok(mut oneshots) = self.oneshots.lock() {
                    for handle in oneshots.drain(..) {
                        handle.abort();
                    }
                }
                let failed = self.inner.lock().ok().and_then(|mut guard| guard.take());
                if let Some(mut scheduler) = failed {
                    let _ = scheduler.shutdown().await;
                }
                self.started.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    async fn start_inner(&self) -> AppResult<()> {
        let ctx = self.exec_context();
        let mut cron_jobs = Vec::new();
        let mut spawned = Vec::new();

        {
            let mut registrations = self.lock_registrations()?;
            for registration in registrations.iter_mut() {
                match &registration.spec {
                    JobSpec::Cron(expr) => {
                        cron_jobs.push((registration.job.clone(), expr.clone()));
                    }
                    JobSpec::Once(delay) => {
                        // shutdown 后再 start 不应重跑「一次性」任务
                        if registration.consumed {
                            continue;
                        }
                        registration.consumed = true;
                        let job = registration.job.clone();
                        let ctx = ctx.clone();
                        let delay = *delay;
                        let drain = self.drain_signal.clone();
                        spawned.push(tokio::spawn(async move {
                            tokio::select! {
                                () = tokio::time::sleep(delay) => {}
                                () = drain.notified() => return,
                            }
                            execute(job, ctx).await;
                        }));
                    }
                }
            }
        }

        if !spawned.is_empty() {
            self.lock_oneshots()?.extend(spawned);
        }

        if !cron_jobs.is_empty() {
            warn_dst_hazard(&self.config.timezone, &cron_jobs);

            let scheduler = JobScheduler::new().await.map_err(|error| {
                AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    format!("创建调度器失败：{error}"),
                )
            })?;

            for (job, expr) in cron_jobs {
                let cron_job = self.build_cron_job(job, &expr, &ctx)?;
                scheduler.add(cron_job).await.map_err(|error| {
                    AppError::biz(
                        ErrorCode::SERVICE_UNAVAILABLE,
                        format!("注册任务失败：{error}"),
                    )
                })?;
            }

            scheduler.start().await.map_err(|error| {
                AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    format!("启动调度器失败：{error}"),
                )
            })?;

            *self.lock_inner()? = Some(scheduler);
        }

        Ok(())
    }

    /// 停止调度（幂等；应在应用优雅关闭时调用）。
    ///
    /// 顺序：停新触发 → 唤醒未到期的一次性任务 → 等 `drain_timeout` 让在途任务收敛 →
    /// 强制取消剩余 → 停 cron 运行时。**不要**跳过这一步：漏调会留下在跑的
    /// 后台任务，且选主锁只能靠 TTL 过期释放。
    pub async fn shutdown(&self) {
        self.draining.store(true, Ordering::Release);
        // 还在 sleep 的一次性任务：立即放弃（错过的触发不补偿）
        self.drain_signal.notify_waiters();

        if !self.inflight.is_empty() {
            let drained = tokio::time::timeout(self.config.drain_timeout, self.wait_idle())
                .await
                .is_ok();
            if !drained {
                tracing::warn!(
                    timeout = ?self.config.drain_timeout,
                    in_flight = self.inflight.len(),
                    "等待在途任务收敛超时，强制取消"
                );
            }
        }

        // 到此为止：已完成的任务句柄无事可做，超时未完成的在这里被取消
        if let Ok(mut oneshots) = self.oneshots.lock() {
            for handle in oneshots.drain(..) {
                handle.abort();
            }
        }

        let scheduler = self.lock_inner().ok().and_then(|mut guard| guard.take());
        if let Some(mut scheduler) = scheduler {
            let _ = scheduler.shutdown().await;
        }

        self.started.store(false, Ordering::Release);
        tracing::info!("调度器已停止");
    }

    /// 等到进程内没有在途任务。
    async fn wait_idle(&self) {
        loop {
            if self.inflight.is_empty() {
                return;
            }
            // 先注册再复查，避免「检查完就变空」时错过通知
            let notified = self.inflight.idle.notified();
            if self.inflight.is_empty() {
                return;
            }
            notified.await;
        }
    }

    fn exec_context(&self) -> ExecContext {
        ExecContext {
            election: self.election.clone(),
            inflight: self.inflight.clone(),
            stats: self.stats.clone(),
            job_timeout: (!self.config.job_timeout.is_zero()).then_some(self.config.job_timeout),
            lock_ttl: self.config.distributed.lock_ttl,
            on_lock_error: self.config.distributed.on_lock_error,
            draining: self.draining.clone(),
            overlap_warn: self.overlap_warn.clone(),
        }
    }

    /// 构造底层 cron 任务（把 [`Job`] 包成 `tokio-cron-scheduler` 的闭包）。
    fn build_cron_job(
        &self,
        job: Arc<dyn Job>,
        expr: &str,
        ctx: &ExecContext,
    ) -> AppResult<CronJob> {
        let ctx = ctx.clone();
        let expr_owned = expr.to_string();

        CronJob::new_async_tz(expr_owned.clone(), self.timezone, move |_uuid, _lock| {
            let job = job.clone();
            let ctx = ctx.clone();
            Box::pin(async move { execute(job, ctx).await })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        })
        .map_err(|error| {
            AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("cron 任务 {expr_owned} 调度失败：{error}"),
            )
        })
    }

    fn push<J: Job>(&self, job: J, spec: JobSpec) -> AppResult<&Self> {
        let mut registrations = self.lock_registrations()?;
        if self.is_started() {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                "调度已启动，不能再注册任务（请在 start() 之前完成注册）",
            ));
        }
        let name = job.name().to_string();
        if registrations
            .iter()
            .any(|registration| registration.job.name() == name)
        {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!(
                    "任务名 \"{name}\" 已存在：选主锁键会冲突，两个任务会互相顶掉。\
                     请用 register_named() 指定唯一名称"
                ),
            ));
        }
        registrations.push(Registration {
            job: Arc::new(job),
            spec,
            consumed: false,
        });
        Ok(self)
    }

    fn lock_registrations(&self) -> AppResult<std::sync::MutexGuard<'_, Vec<Registration>>> {
        self.registrations
            .lock()
            .map_err(|_| AppError::biz(ErrorCode::INTERNAL, "任务注册表被污染"))
    }

    fn lock_inner(&self) -> AppResult<std::sync::MutexGuard<'_, Option<JobScheduler>>> {
        self.inner
            .lock()
            .map_err(|_| AppError::biz(ErrorCode::INTERNAL, "调度器状态被污染"))
    }

    fn lock_oneshots(&self) -> AppResult<std::sync::MutexGuard<'_, Vec<JoinHandle<()>>>> {
        self.oneshots
            .lock()
            .map_err(|_| AppError::biz(ErrorCode::INTERNAL, "一次性任务表被污染"))
    }
}

/// 执行一次任务：进程内互斥 → 选主 → 执行（带超时与续期）→ 记日志。
async fn execute(job: Arc<dyn Job>, ctx: ExecContext) {
    let name = job.name().to_string();

    if ctx.draining.load(Ordering::Acquire) {
        ctx.stats.skipped_draining.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(job = %name, outcome = "draining", "调度器关闭中，跳过本次触发");
        return;
    }

    // 闸门 1：进程内同名互斥（distributed 关闭时这是唯一的「不重叠」保证）
    let Some(_slot) = ctx.inflight.try_enter(&name) else {
        ctx.stats.skipped_overlap.fetch_add(1, Ordering::Relaxed);
        if ctx.overlap_warn.allow() {
            tracing::warn!(
                job = %name,
                outcome = "overlap",
                "同名任务上一次尚未结束，已跳过本次触发（累计见 Scheduler::stats）"
            );
        } else {
            tracing::debug!(job = %name, outcome = "overlap", "同名任务重叠，跳过本次触发");
        }
        return;
    };

    // 闸门 2：跨实例选主
    let mut guard: Option<DistributedLock> = None;
    if let Some(election) = &ctx.election {
        match election.try_acquire(&name).await {
            Ok(Some(acquired)) => guard = Some(acquired),
            Ok(None) => {
                ctx.stats.skipped_not_leader.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    job = %name,
                    outcome = "not_leader",
                    "其他实例持有选主锁，跳过本次触发"
                );
                return;
            }
            Err(error) => {
                ctx.stats.lock_errors.fetch_add(1, Ordering::Relaxed);
                match ctx.on_lock_error {
                    LockErrorPolicy::Skip => {
                        tracing::error!(
                            %error,
                            job = %name,
                            outcome = "lock_error",
                            "选主失败，跳过本次触发（[scheduler].distributed.on_lock_error = \"skip\"）"
                        );
                        return;
                    }
                    LockErrorPolicy::Run => {
                        tracing::error!(
                            %error,
                            job = %name,
                            outcome = "lock_error",
                            "选主失败，按配置继续执行（可能与其他实例重复执行）"
                        );
                    }
                }
            }
        }
    }

    // 闸门 3：执行（超时 + 续期看门狗 + panic 隔离）
    let started_at = Instant::now();
    let outcome = run_with_watchdog(job, &name, guard.as_ref(), &ctx).await;
    let elapsed_ms = started_at.elapsed().as_millis();

    match outcome {
        Outcome::Completed(Ok(())) => {
            ctx.stats.runs.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(job = %name, outcome = "ok", elapsed_ms, "任务执行完成");
        }
        Outcome::Completed(Err(error)) => {
            ctx.stats.runs.fetch_add(1, Ordering::Relaxed);
            ctx.stats.failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(%error, job = %name, outcome = "failed", elapsed_ms, "任务执行失败");
        }
        Outcome::Timeout => {
            ctx.stats.timeouts.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                job = %name,
                outcome = "timeout",
                elapsed_ms,
                timeout = ?ctx.job_timeout,
                "任务执行超时，已取消（[scheduler].job_timeout）"
            );
        }
        Outcome::LeaderLost => {
            ctx.stats.leader_lost.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                job = %name,
                outcome = "leader_lost",
                elapsed_ms,
                "执行期间失去选主锁，任务已取消（避免与其他实例双跑）"
            );
        }
        Outcome::Panicked(message) => {
            ctx.stats.panics.fetch_add(1, Ordering::Relaxed);
            tracing::error!(job = %name, outcome = "panic", elapsed_ms, panic = %message, "任务 panic");
        }
        Outcome::Cancelled => {
            tracing::warn!(job = %name, outcome = "cancelled", elapsed_ms, "任务被取消");
        }
    }
    // guard 在此 drop：释放选主锁（token 不匹配时不会误删他人的锁）
}

/// 跑任务体并返回结局。
///
/// 有选主锁时同时驱动续期看门狗：`lock_ttl / 3` 续一次，
/// 续期返回 `false`（锁已不属于自己）或「续期持续失败超过 TTL」时立即取消任务。
async fn run_with_watchdog(
    job: Arc<dyn Job>,
    name: &str,
    guard: Option<&DistributedLock>,
    ctx: &ExecContext,
) -> Outcome {
    let mut handle = AbortOnDrop(tokio::spawn(async move { job.run().await }));

    let Some(guard) = guard else {
        return wait_job(&mut handle, ctx.job_timeout).await;
    };

    let ttl = ctx.lock_ttl;
    let renew_every = (ttl / 3).max(Duration::from_millis(100));
    let mut ticker = tokio::time::interval(renew_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // 丢掉 interval 的第一个「立即就绪」tick

    let timeout_fut = async {
        match ctx.job_timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout_fut);

    let mut last_renewed = Instant::now();

    loop {
        tokio::select! {
            joined = &mut handle.0 => return join_outcome(joined),
            () = &mut timeout_fut => return Outcome::Timeout,
            _ = ticker.tick() => {
                match guard.renew(Some(ttl), ttl).await {
                    Ok(true) => last_renewed = Instant::now(),
                    Ok(false) => {
                        tracing::error!(
                            job = %name,
                            outcome = "renew_lost",
                            "锁续期发现所有权已丢失（TTL 过期后被抢占）"
                        );
                        return Outcome::LeaderLost;
                    }
                    Err(error) => {
                        // 续期请求失败不等于失锁：只有「自上次成功续期已超过 TTL」才判定失锁
                        if last_renewed.elapsed() >= ttl {
                            tracing::error!(
                                %error,
                                job = %name,
                                outcome = "renew_lost",
                                "锁续期持续失败且已超过 TTL，判定失去选主锁"
                            );
                            return Outcome::LeaderLost;
                        }
                        tracing::warn!(%error, job = %name, outcome = "renew_failed", "锁续期失败，下个周期重试");
                    }
                }
            }
        }
    }
}

/// 无选主锁时的等待：只需要超时。
async fn wait_job(handle: &mut AbortOnDrop<AppResult<()>>, timeout: Option<Duration>) -> Outcome {
    let Some(timeout) = timeout else {
        return join_outcome((&mut handle.0).await);
    };
    match tokio::time::timeout(timeout, &mut handle.0).await {
        Ok(joined) => join_outcome(joined),
        Err(_) => Outcome::Timeout,
    }
}

/// 把 `JoinHandle` 的结局映射成 [`Outcome`]（panic 在这里被捕获并记录，不会带走调度器）。
fn join_outcome(joined: Result<AppResult<()>, JoinError>) -> Outcome {
    match joined {
        Ok(result) => Outcome::Completed(result),
        Err(error) => {
            if !error.is_panic() {
                return Outcome::Cancelled;
            }
            let payload = error.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "非字符串 panic".to_string());
            Outcome::Panicked(message)
        }
    }
}

/// 把闭包适配成 [`JobHandler`]。
fn boxed<F, Fut>(handler: F) -> JobHandler
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
{
    Arc::new(move || {
        let future: JobFuture = Box::pin(handler());
        future
    })
}

/// 校验任务名：非空、≤128 字节、无控制字符。
///
/// 名字会进入锁键与日志：控制字符会污染日志（伪造行）并让锁键不可读，
/// 过长会让 `cache` 键超过 200 字节上限——那种情况下 `lock()` 直接报错，
/// 任务会**永远静默不执行**。
fn validate_job_name(name: &str) -> AppResult<()> {
    if name.is_empty() {
        return Err(AppError::biz(ErrorCode::INVALID_PARAM, "任务名不能为空"));
    }
    if name.len() > MAX_JOB_NAME_BYTES {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            format!("任务名超过 {MAX_JOB_NAME_BYTES} 字节上限"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "任务名不能包含控制字符（会污染日志与锁键）",
        ));
    }
    Ok(())
}

/// 用底层库校验 cron 表达式（6 段含秒）。
fn validate_cron(expr: &str, timezone: Tz) -> AppResult<()> {
    CronJob::new_async_tz(expr, timezone, |_uuid, _lock| Box::pin(async {}))
        .map(|_| ())
        .map_err(|error| {
            AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("非法 cron 表达式 \"{expr}\"（需 6 段含秒）：{error}"),
            )
        })
}

/// 有夏令时的时区 + 固定时刻表达式 → 切换日会漏跑/双跑，启动期提醒一次。
fn warn_dst_hazard(timezone: &str, jobs: &[(Arc<dyn Job>, String)]) {
    let Ok(tz) = timezone.parse::<Tz>() else {
        return;
    };
    if !has_dst(tz) {
        return;
    }
    for (job, expr) in jobs {
        if !is_fixed_hour(expr) {
            continue;
        }
        tracing::warn!(
            job = %job.name(),
            timezone = %timezone,
            expr = %expr,
            "时区有夏令时，固定时刻的 cron 在切换日可能漏跑或重复执行；\
             安全任务建议用 UTC + 小时级频率表达式（如 0 0 * * * *）"
        );
    }
}

/// 该时区一年内是否存在偏移变化（粗判：1 月与 7 月偏移不同即视为有 DST）。
fn has_dst(tz: Tz) -> bool {
    use chrono::{Offset, TimeZone};

    let offset_of = |month: u32| {
        let date = chrono::NaiveDate::from_ymd_opt(2024, month, 1)?;
        let naive = date.and_hms_opt(0, 0, 0)?;
        Some(tz.offset_from_utc_datetime(&naive).fix())
    };
    match (offset_of(1), offset_of(7)) {
        (Some(january), Some(july)) => january != july,
        _ => false,
    }
}

/// 小时字段是否写死（不含 `*`）——这类表达式最容易受 DST 影响。
fn is_fixed_hour(expr: &str) -> bool {
    expr.split_whitespace()
        .nth(2)
        .is_some_and(|hour| !hour.contains('*'))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use crate::cache::MemoryCache;
    use crate::config::CacheSection;
    use crate::config::sections::scheduler::DistributedSection;

    use super::*;

    fn cache() -> CacheService {
        CacheService::new(Arc::new(MemoryCache::default()), CacheSection::default())
    }

    fn scheduler(config: SchedulerSection) -> Scheduler {
        Scheduler::from_config(&config, cache()).unwrap()
    }

    fn config(enabled: bool) -> SchedulerSection {
        SchedulerSection {
            enabled,
            ..SchedulerSection::default()
        }
    }

    fn distributed_config() -> SchedulerSection {
        SchedulerSection {
            distributed: DistributedSection {
                enabled: true,
                lock_ttl: Duration::from_secs(1),
                on_lock_error: LockErrorPolicy::Skip,
            },
            ..SchedulerSection::default()
        }
    }

    fn counter(scheduler: &Scheduler, spec: JobSpec) -> Arc<AtomicUsize> {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        scheduler
            .register(
                FnJob::new(
                    "test-job",
                    Arc::new(move || {
                        let hits = handler_hits.clone();
                        Box::pin(async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                    }),
                ),
                spec,
            )
            .unwrap();
        hits
    }

    #[test]
    fn should_register_cron_and_once() {
        let scheduler = scheduler(config(true));
        scheduler
            .register_cron("0 0 3 * * *", || async { Ok(()) })
            .unwrap();
        scheduler
            .register_once(Duration::from_secs(1), || async { Ok(()) })
            .unwrap();

        let names = scheduler.job_names();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "cron:0 0 3 * * *");
        assert_eq!(names[1], "once:0");
    }

    #[test]
    fn should_reject_invalid_cron() {
        let scheduler = scheduler(config(true));
        let error = scheduler
            .register_cron("not a cron", || async { Ok(()) })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
        assert!(scheduler.job_names().is_empty(), "失败不应留下注册项");
    }

    /// 同一表达式两个任务：锁键相同，过去会「静默只有一个跑」，现在注册期就拒绝。
    #[test]
    fn should_reject_duplicate_job_name() {
        let scheduler = scheduler(config(true));
        scheduler
            .register_cron("0 0 3 * * *", || async { Ok(()) })
            .unwrap();
        let error = scheduler
            .register_cron("0 0 3 * * *", || async { Ok(()) })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
        assert!(format!("{error}").contains("已存在"), "错误信息应说明重名");
        assert_eq!(scheduler.job_names().len(), 1);
    }

    /// 需要同刻跑多个任务时，用 register_named 给唯一名字。
    #[test]
    fn should_allow_distinct_names_on_same_schedule() {
        let scheduler = scheduler(config(true));
        let spec = || JobSpec::Cron("0 0 3 * * *".to_string());
        scheduler
            .register_named("cleanup-sessions", spec(), || async { Ok(()) })
            .unwrap();
        scheduler
            .register_named("cleanup-audit", spec(), || async { Ok(()) })
            .unwrap();
        assert_eq!(scheduler.job_names().len(), 2);
    }

    #[test]
    fn should_reject_control_chars_in_job_name() {
        let scheduler = scheduler(config(true));
        let error = scheduler
            .register_named(
                "cleanup\nforged-line",
                JobSpec::Once(Duration::from_secs(1)),
                || async { Ok(()) },
            )
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
    }

    #[test]
    fn should_reject_overlong_job_name() {
        let scheduler = scheduler(config(true));
        let name = "x".repeat(MAX_JOB_NAME_BYTES + 1);
        let error = scheduler
            .register_named(&name, JobSpec::Once(Duration::from_secs(1)), || async {
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
    }

    #[test]
    fn should_reject_invalid_timezone() {
        let mut config = config(true);
        config.timezone = "Mars/Olympus".to_string();
        assert!(Scheduler::from_config(&config, cache()).is_err());
    }

    #[test]
    fn should_enable_leader_election_only_when_configured() {
        assert!(scheduler(config(true)).election.is_none());
        assert!(scheduler(distributed_config()).election.is_some());
    }

    #[tokio::test]
    async fn should_run_one_shot_job_once() {
        let scheduler = scheduler(config(true));
        let hits = counter(&scheduler, JobSpec::Once(Duration::from_millis(50)));

        scheduler.start().await.unwrap();
        assert!(scheduler.is_started());
        tokio::time::sleep(Duration::from_millis(300)).await;
        scheduler.shutdown().await;

        assert_eq!(hits.load(Ordering::SeqCst), 1, "一次性任务应执行一次");
    }

    #[tokio::test]
    async fn should_run_cron_job_repeatedly() {
        let scheduler = scheduler(config(true));
        let hits = counter(&scheduler, JobSpec::Cron("*/1 * * * * *".to_string()));

        scheduler.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        scheduler.shutdown().await;

        let fired = hits.load(Ordering::SeqCst);
        assert!(fired >= 1, "cron 任务应至少执行一次，实际 {fired}");
    }

    #[tokio::test]
    async fn should_abort_pending_one_shot_on_shutdown() {
        let scheduler = scheduler(config(true));
        let hits = counter(&scheduler, JobSpec::Once(Duration::from_secs(5)));

        scheduler.start().await.unwrap();
        scheduler.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "关闭时应取消未触发的一次性任务"
        );
    }

    #[tokio::test]
    async fn should_not_start_when_disabled() {
        let scheduler = scheduler(config(false));
        let hits = counter(&scheduler, JobSpec::Once(Duration::from_millis(10)));

        scheduler.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(!scheduler.is_started(), "禁用时不应启动");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn should_reject_register_after_start() {
        let scheduler = scheduler(config(true));
        scheduler.start().await.unwrap();
        let error = scheduler
            .register_cron("0 0 3 * * *", || async { Ok(()) })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::INVALID_PARAM);
        assert!(format!("{error}").contains("已启动"));
        scheduler.shutdown().await;
    }

    /// F7：shutdown 后再 start，一次性任务不应重跑。
    #[tokio::test]
    async fn should_not_rerun_one_shot_after_restart() {
        let scheduler = scheduler(config(true));
        let hits = counter(&scheduler, JobSpec::Once(Duration::from_millis(20)));

        scheduler.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        scheduler.shutdown().await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        scheduler.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        scheduler.shutdown().await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "一次性任务不应在重启后被重新 spawn"
        );
    }

    /// F3：同名任务重叠时跳过，不并发执行。
    #[tokio::test]
    async fn should_skip_overlapping_execution() {
        let scheduler = scheduler(config(true));
        let ctx = scheduler.exec_context();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let job = {
            let running = running.clone();
            let peak = peak.clone();
            Arc::new(FnJob::new(
                "slow",
                Arc::new(move || {
                    let running = running.clone();
                    let peak = peak.clone();
                    Box::pin(async move {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
            )) as Arc<dyn Job>
        };

        let first = execute(job.clone(), ctx.clone());
        let second = execute(job, ctx.clone());
        let (_, ()) = tokio::join!(first, second);

        assert_eq!(peak.load(Ordering::SeqCst), 1, "同名任务不应并发执行");
        assert_eq!(scheduler.stats().skipped_overlap.load(Ordering::Relaxed), 1);
    }

    /// F2：超过 job_timeout 的任务被取消。
    #[tokio::test]
    async fn should_cancel_job_after_timeout() {
        let mut config = config(true);
        config.job_timeout = Duration::from_millis(100);
        let scheduler = scheduler(config);

        let finished = Arc::new(AtomicUsize::new(0));
        let job = {
            let finished = finished.clone();
            Arc::new(FnJob::new(
                "stuck",
                Arc::new(move || {
                    let finished = finished.clone();
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        finished.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
            )) as Arc<dyn Job>
        };

        execute(job, scheduler.exec_context()).await;

        assert_eq!(finished.load(Ordering::SeqCst), 0, "超时任务应被取消");
        assert_eq!(scheduler.stats().timeouts.load(Ordering::Relaxed), 1);
    }

    /// F1：任务比 lock_ttl 长时，看门狗续期让锁一直被持有。
    #[tokio::test]
    async fn should_renew_lock_while_job_runs() {
        let mut config = distributed_config();
        config.distributed.lock_ttl = Duration::from_secs(1);
        let scheduler = scheduler(config);
        let ctx = scheduler.exec_context();

        let observed = Arc::new(AtomicUsize::new(0));
        let election = scheduler.election.clone().unwrap();
        let job = {
            let observed = observed.clone();
            let election = election.clone();
            Arc::new(FnJob::new(
                "long-job",
                Arc::new(move || {
                    let observed = observed.clone();
                    let election = election.clone();
                    Box::pin(async move {
                        // 越过一个 TTL 后，锁仍应属于本次执行
                        tokio::time::sleep(Duration::from_millis(1400)).await;
                        if election.try_acquire("long-job").await.unwrap().is_none() {
                            observed.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(())
                    })
                }),
            )) as Arc<dyn Job>
        };

        execute(job, ctx).await;

        assert_eq!(
            observed.load(Ordering::SeqCst),
            1,
            "执行期间锁应被续期，其他实例抢不到"
        );
    }

    /// panic 不应带走调度器，且要计入 stats。
    #[tokio::test]
    async fn should_survive_job_panic() {
        let scheduler = scheduler(config(true));
        let job = Arc::new(FnJob::new(
            "boom",
            Arc::new(|| Box::pin(async { panic!("job exploded") })),
        )) as Arc<dyn Job>;

        execute(job, scheduler.exec_context()).await;

        assert_eq!(scheduler.stats().panics.load(Ordering::Relaxed), 1);
    }

    /// F8：关闭时等在途任务跑完再返回。
    #[tokio::test]
    async fn should_drain_running_job_on_shutdown() {
        let scheduler = scheduler(config(true));
        let hits = counter(&scheduler, JobSpec::Once(Duration::from_millis(20)));

        scheduler.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        scheduler.shutdown().await;

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.inflight.len(), 0, "关闭后不应留下在途任务");
    }

    #[test]
    fn should_detect_fixed_hour_expressions() {
        assert!(is_fixed_hour("0 0 3 * * *"));
        assert!(is_fixed_hour("0 30 2,3 * * *"));
        assert!(!is_fixed_hour("0 0 * * * *"));
        assert!(!is_fixed_hour("0 */5 * * * *"));
    }

    #[test]
    fn should_detect_dst_timezones() {
        assert!(has_dst("America/New_York".parse().unwrap()));
        assert!(!has_dst("UTC".parse().unwrap()));
        assert!(!has_dst("Asia/Shanghai".parse().unwrap()));
    }
}
