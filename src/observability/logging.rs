//! tracing 初始化（文档 三·8）：**分级**（`RUST_LOG` > `[log].level`）、
//! **结构化输出**（console / JSON，JSON 便于采集进 ELK / Loki）、
//! 输出目标 stdout +（feature = "log-file"）滚动文件可并存。
//! feature = "otel" 时将 OpenTelemetry layer 一并挂到订阅链上。
//!
//! 进程内重复 init 安全（第二次起为 no-op，测试多次装配不受影响）。

use std::io::Write;

use tracing_subscriber::{prelude::*, registry, EnvFilter};

use crate::config::Settings;

/// 输出目的地（console / file 共用同一 fmt layer 类型，便于链式组合）：
/// - `Stdout`：控制台输出（保持 ANSI 着色）；
/// - `File`：rotate-rs 滚动文件（关闭 ANSI）；
/// - `Sink`：黑洞（对应层未启用时的占位，写入即丢弃）。
enum LogSink {
    Stdout,
    #[cfg(feature = "log-file")]
    File(file_writer::SharedRotatingWriter),
    Sink,
}

impl LogSink {
    fn stdout() -> Self {
        Self::Stdout
    }

    #[cfg(feature = "log-file")]
    fn file_or_sink(w: Option<file_writer::SharedRotatingWriter>) -> Self {
        match w {
            Some(w) => Self::File(w),
            None => Self::Sink,
        }
    }
}

struct SinkWriter<'a>(&'a LogSink);

impl Write for SinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0 {
            LogSink::Stdout => std::io::stdout().write_all(buf).map(|_| buf.len()),
            LogSink::Sink => Ok(buf.len()),
            #[cfg(feature = "log-file")]
            LogSink::File(w) => w.locked().write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0 {
            LogSink::Stdout => std::io::stdout().flush(),
            LogSink::Sink => Ok(()),
            #[cfg(feature = "log-file")]
            LogSink::File(w) => w.locked().flush(),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = SinkWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter(self)
    }
}

/// 日志后端保活句柄（持有到进程结束；Drop 时 flush 各后端缓冲）
pub struct LogGuard {
    #[cfg(feature = "otel")]
    _otel: Option<crate::observability::tracing::OtelGuard>,
    /// log-file feature：保活 rotate-rs writer（存活即后台 worker 存活）。
    /// `Box<dyn Any>` 仅用于抹平 feature 组合的类型差异。
    _file: Option<Box<dyn std::any::Any + Send + Sync>>,
}

#[cfg(feature = "log-file")]
mod file_writer {
    //! rotate-rs `Writer` → 共享句柄。Writer 的 `write` 需要 `&mut self`，
    //! 跨线程共享用 Mutex 包一层（NonBlocking 模式下 write 只是入队，锁竞争可忽略）。

    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    /// 共享句柄：fmt layer 与 [`LogGuard`](super::LogGuard) 各持一份，
    /// 全部 Drop 后 rotate-rs worker 排空退出
    #[derive(Clone)]
    pub struct SharedRotatingWriter(pub(crate) Arc<Mutex<rotate_rs::Writer>>);

    impl SharedRotatingWriter {
        /// 毒化恢复：临界区内只有 rotate-rs 内部逻辑，无用户代码可 panic
        pub(crate) fn locked(&self) -> MutexGuard<'_, rotate_rs::Writer> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }
}

/// 由 `[log.file]` 配置构造 rotate-rs 滚动写入器（进程内只调用一次）
#[cfg(feature = "log-file")]
fn build_rotating_writer(
    cfg: &crate::config::sections::FileLogSettings,
) -> file_writer::SharedRotatingWriter {
    use std::time::Duration;

    let max_size = (cfg.max_size_mb.max(1) as usize).saturating_mul(1024 * 1024);
    let interval = Duration::from_secs(cfg.interval_secs.max(1));
    let rotation = match cfg.rotation.as_str() {
        "size" => rotate_rs::Rotation::Size(max_size),
        "time" => rotate_rs::Rotation::Time(interval),
        _ => rotate_rs::Rotation::Hybrid { max_size, interval },
    };
    let mode = if cfg.non_blocking {
        rotate_rs::Mode::NonBlocking(rotate_rs::NonBlockingConfig {
            // 0 = 使用 rotate-rs 默认容量
            channel_capacity: 0,
            max_pending_bytes: 0,
            overflow: if cfg.overflow == "drop" {
                rotate_rs::OverflowStrategy::DropNew
            } else {
                rotate_rs::OverflowStrategy::Block
            },
        })
    } else {
        rotate_rs::Mode::Sync
    };

    let rcfg = rotate_rs::Config {
        dir: cfg.dir.clone(),
        name: cfg.name.clone(),
        suffix: "log".to_string(),
        sep: ".".to_string(),
        // rotate-rs 要求时间串不含字母/路径分隔符（文件名可解析性约束）
        date_format: "%Y-%m-%d-%H-%M-%S".to_string(),
        template: "{dir}/{name}{sep}{date}{sep}{counter}{sep}{suffix}".to_string(),
        rotation,
        max_backups: if cfg.max_backups == 0 {
            None
        } else {
            Some(cfg.max_backups)
        },
        compress: cfg.compress,
        mode,
        // rotate-rs 自带 stderr 节流输出；回调里不能打日志（递归），保持 None
        error_handler: None,
    };

    match rotate_rs::open(rcfg) {
        Ok(w) => file_writer::SharedRotatingWriter(std::sync::Arc::new(std::sync::Mutex::new(w))),
        // 启动期基础设施故障：fail-fast
        Err(e) => panic!(
            "log file init failed (dir={}, name={}): {e}",
            cfg.dir, cfg.name
        ),
    }
}

/// 文件输出开关：`RUST_LOG_FILE=1` 环境变量或 `[log.file].enabled`
#[cfg(feature = "log-file")]
fn file_enabled(cfg: &Settings) -> bool {
    if std::env::var("RUST_LOG_FILE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return true;
    }
    cfg.log.file.enabled
}

/// 初始化全局 tracing。重复调用安全：已初始化时本进程内为 no-op（返回占位 guard）。
pub fn init(cfg: &Settings) -> LogGuard {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.log.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    #[cfg(feature = "log-file")]
    let shared_file: Option<file_writer::SharedRotatingWriter> = if file_enabled(cfg) {
        let w = build_rotating_writer(&cfg.log.file);
        tracing::info!(
            dir = %cfg.log.file.dir,
            name = %cfg.log.file.name,
            "log file rotation enabled"
        );
        Some(w)
    } else {
        None
    };

    let file_keepalive: Option<Box<dyn std::any::Any + Send + Sync>>;
    #[cfg(feature = "log-file")]
    {
        file_keepalive = shared_file.as_ref().map(|w| Box::new(w.0.clone()) as _);
    }
    #[cfg(not(feature = "log-file"))]
    {
        file_keepalive = None;
    }

    let console_sink = LogSink::stdout();
    #[cfg(feature = "log-file")]
    let file_sink = LogSink::file_or_sink(shared_file.clone());
    #[cfg(not(feature = "log-file"))]
    let file_sink = LogSink::Sink;

    // 按格式分派：同一分支内 console / file 两个 fmt layer 具体类型一致，可直接
    // 链式组合（fmt::Layer 的 Layer 实现绑定构造时的 Subscriber，无法跨格式混搭）
    match cfg.log.format.as_str() {
        "json" => {
            let console = cfg
                .log
                .stdout
                .then(|| tracing_subscriber::fmt::layer().json().with_writer(console_sink));
            let file = tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(file_sink);
            mount(cfg, registry().with(console).with(file), filter)
        }
        _ => {
            let console = cfg
                .log
                .stdout
                .then(|| tracing_subscriber::fmt::layer().with_writer(console_sink));
            let file = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_sink);
            mount(cfg, registry().with(console).with(file), filter)
        }
    };

    LogGuard {
        #[cfg(feature = "otel")]
        _otel: None, // otel guard 由 GLOBAL_OTEL_GUARD 静态保活
        _file: file_keepalive,
    }
}

/// 挂载 otel 层与过滤器；已初始化时 no-op
#[allow(unused_variables)] // cfg 参数：无 otel feature 时用不到
fn mount<S>(
    cfg: &Settings,
    subscriber: S,
    filter: EnvFilter,
) -> tracing::Dispatch
where
    S: tracing::Subscriber
        + Send
        + Sync
        + 'static
        + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    #[cfg(feature = "otel")]
    {
        let service_name = if cfg.log.service_name.is_empty() {
            "core-rs".to_string()
        } else {
            cfg.log.service_name.clone()
        };
        match crate::observability::tracing::setup_layer::<S>(&service_name) {
            Some((otel_layer, guard)) => {
                let dispatch = tracing::Dispatch::new(subscriber.with(otel_layer).with(filter));
                if tracing::dispatcher::set_global_default(dispatch.clone()).is_err() {
                    return tracing::dispatcher::get_default(|d| d.clone());
                }
                GLOBAL_OTEL_GUARD.set(guard).ok();
                return dispatch;
            }
            None => {
                let dispatch = tracing::Dispatch::new(subscriber.with(filter));
                if tracing::dispatcher::set_global_default(dispatch.clone()).is_err() {
                    return tracing::dispatcher::get_default(|d| d.clone());
                }
                return dispatch;
            }
        }
    }

    #[cfg(not(feature = "otel"))]
    {
        let dispatch = tracing::Dispatch::new(subscriber.with(filter));
        if tracing::dispatcher::set_global_default(dispatch.clone()).is_err() {
            return tracing::dispatcher::get_default(|d| d.clone());
        }
        dispatch
    }
}

/// otel guard 保活（进程级，Drop 时 flush spans）
#[cfg(feature = "otel")]
static GLOBAL_OTEL_GUARD: std::sync::OnceLock<crate::observability::tracing::OtelGuard> =
    std::sync::OnceLock::new();
