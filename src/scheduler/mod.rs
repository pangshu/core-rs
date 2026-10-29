//! 任务调度（feature `task`）。
//!
//! - [`Scheduler`]：装配入口（`AppState.scheduler`），`register_cron` / `register_once` /
//!   `register_named` / `register` / `start` / `shutdown` / `stats`
//! - [`Job`] / [`JobSpec`]：自定义任务与调度规格
//! - [`LeaderElection`]：多实例选主（复用 `cache` 分布式锁，**每次触发选一次**，
//!   执行期间由看门狗续期）
//!
//! **仅内存调度**：任务在代码里声明，不建表持久化；进程重启后按注册重新调度，
//! 停机期间错过的触发**不补偿**（见 `docs/03` §4.13）。
//!
//! **每次触发三道闸**：
//! 1. 进程内同名互斥——上一次没跑完就跳过（`distributed` 关闭时同样生效）；
//! 2. 跨实例选主——抢 `sched:{name}` 锁，续期失败到超过 TTL 即取消任务；
//! 3. 执行超时——`job_timeout` 到点取消，避免卡死任务占锁并堆积。
//!
//! **任务名 = 选主锁键**：`register_cron` 用 `cron:{expr}`，同一表达式只能注册一次；
//! 同刻要跑多个任务时用 [`Scheduler::register_named`] 显式命名（重名在注册期报错）。
//!
//! **优雅关闭**：应用应在 `serve()` 返回后调用
//! [`Scheduler::shutdown`](Scheduler::shutdown)（关闭顺序见 `docs/02` §7：
//! realtime → HTTP → queue → **scheduler** → cache/db）。`shutdown` 会先停新触发，
//! 再等 `drain_timeout` 让在途任务收敛，最后才强制取消。
//!
//! **不做**：不做分布式任务编排 / 任务持久化 / 历史补跑。

pub mod distributed;
pub mod job;
pub mod runner;

pub use distributed::LeaderElection;
pub use job::{Job, JobSpec};
pub use runner::{ExecStats, Scheduler};
