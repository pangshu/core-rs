//! tracing 初始化：level 优先级为 `RUST_LOG` 环境变量 > 配置文件 `[log].level`。
//! format 支持 `console`（人类可读）与 `json`（采集友好）。
//! feature = "log-file" 时可同时落盘到滚动文件（rotate-rs：时间/大小/混合轮转 + gz 压缩）。
//! feature = "otel" 时将 OpenTelemetry layer 一并挂到订阅链上。

use std::io::Write;

#[cfg(feature = "log-file")]
use std::sync::{Arc, Mutex};

use tracing_subscriber::{prelude::*, registry, EnvFilter};

use crate::config::AppConfig;

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

    /// log-file feature 下按开关返回 File 或 Sink
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

/// 返回 [`LogGuard`]，调用方需持有到进程结束（Drop 时 flush 各后端缓冲）。
pub struct LogGuard {
    #[cfg(feature = "otel")]
    _otel: Option<crate::observe::otel::OtelGuard>,
    /// log-file feature：保活 rotate-rs writer（存活即后台 worker 存活，
    /// Drop 时排空缓冲落盘）。`Box<dyn Any>` 仅用于抹平 feature 组合的类型差异。
    _file: Option<Box<dyn std::any::Any + Send>>,
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
fn build_rotating_writer(cfg: &crate::config::FileLogConfig) -> file_writer::SharedRotatingWriter {
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
        Ok(w) => file_writer::SharedRotatingWriter(Arc::new(Mutex::new(w))),
        // 启动期基础设施故障：fail-fast（设计铁律 4）
        Err(e) => panic!(
            "log file init failed (dir={}, name={}): {e}",
            cfg.dir, cfg.name
        ),
    }
}

/// 文件输出开关：`RUST_LOG_FILE=1` 环境变量或 `[log.file].enabled`
#[cfg(feature = "log-file")]
fn file_enabled(cfg: &AppConfig) -> bool {
    if std::env::var("RUST_LOG_FILE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return true;
    }
    cfg.log.file.enabled
}

pub fn init(cfg: &AppConfig) -> LogGuard {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.log.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // 文件输出只构造一次，fmt layer 与 guard 共享同一 Writer
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

    // rotate-rs writer 保活句柄（无 log-file feature 时恒为 None）
    let file_keepalive: Option<Box<dyn std::any::Any + Send>>;
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
            mount(cfg, registry().with(console).with(file), filter, file_keepalive)
        }
        _ => {
            let console = cfg
                .log
                .stdout
                .then(|| tracing_subscriber::fmt::layer().with_writer(console_sink));
            let file = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_sink);
            mount(cfg, registry().with(console).with(file), filter, file_keepalive)
        }
    }
}

/// 挂载 otel 层与过滤器并 init
#[allow(unused_variables)] // cfg 参数：无 otel feature 时用不到
fn mount<S>(
    cfg: &AppConfig,
    subscriber: S,
    filter: EnvFilter,
    file_keepalive: Option<Box<dyn std::any::Any + Send>>,
) -> LogGuard
where
    S: tracing::Subscriber
        + Send
        + Sync
        + 'static
        + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    #[cfg(feature = "otel")]
    {
        match crate::observe::otel::setup(&cfg.otel) {
            Some((otel_layer, guard)) => {
                subscriber.with(otel_layer).with(filter).init();
                return LogGuard {
                    _otel: Some(guard),
                    _file: file_keepalive,
                };
            }
            None => {
                subscriber.with(filter).init();
                return LogGuard {
                    _otel: None,
                    _file: file_keepalive,
                };
            }
        }
    }

    #[cfg(not(feature = "otel"))]
    {
        subscriber.with(filter).init();
        LogGuard {
            _file: file_keepalive,
        }
    }
}

#[cfg(all(test, feature = "log-file"))]
mod tests {
    use super::*;

    #[test]
    fn rotating_writer_creates_file_and_writes() {
        let dir = std::env::temp_dir().join(format!("core-rs-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = crate::config::FileLogConfig {
            enabled: true,
            dir: dir.to_string_lossy().to_string(),
            name: "app".to_string(),
            // 同步写：write 返回即落盘，可直接断言
            non_blocking: false,
            ..Default::default()
        };
        let writer = build_rotating_writer(&cfg);
        writer.locked().write_all(b"hello rotate\n").unwrap();
        writer.locked().flush().unwrap();

        let found = std::fs::read_dir(&dir)
            .expect("log dir should be created")
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|x| x == "log"));
        assert!(found, "log file should exist under {dir:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
