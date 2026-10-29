//! 滚动文件日志（feature `log-file`）。
//!
//! 基于 [`rotate_rs`]：时间 / 大小 / 混合切割 + gz 压缩 + 非阻塞写入。本模块只做
//! 三件事——把 [`FileSection`] 翻译成 `rotate_rs::Config`、把 `Writer` 包成可跨线程
//! 共享的句柄、在退出时把队列排空。
//!
//! # 为什么是 `Arc<Mutex<Writer>>`
//!
//! `rotate_rs::Writer::write` 需要 `&mut self`，且同步模式没有可克隆的句柄
//! （只有非阻塞模式能克隆 `AsyncWriter`）。统一用一把锁包住两种模式最简单；
//! 非阻塞模式下临界区只是入队，锁竞争可以忽略。
//!
//! # 与 tracing 的衔接
//!
//! 一条 tracing 事件在 fmt 层会分多次 `write`，而 rotate-rs 的原子单位是
//! **单次 write**——逐片直写会在多线程下交织成半行，轮转还可能在片段之间发生
//! 把一条日志拆到两个文件。因此写入口是 [`RotatingFile::write_line`]：调用方
//! （[`crate::telemetry::logging`] 的行缓冲）在事件结束时一次性交来整行。

use std::io::{self, Write as _};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use rotate_rs::{Config, Mode, NonBlockingConfig, OverflowStrategy, Rotation, Writer};

use crate::boot::startup::StartupError;
use crate::config::sections::{FileOverflow, FileRotation, FileSection, FileWriteMode};

// 归档命名：`{dir}/{name}{sep}{date}{sep}{counter}{sep}{suffix}`。
// 注意 `{counter}` 是**重名去重后缀**（同一秒内第二次轮转才补 `.1`），不是序号；
// 因此 `date_format` 固定到秒，且不能含字母（rotate-rs 的解析约束）。
const DATE_FORMAT: &str = "%Y-%m-%d-%H-%M-%S";
const NAME_SEP: &str = ".";
const TEMPLATE: &str = "{dir}/{name}{sep}{date}{sep}{counter}{sep}{suffix}";

/// 进程级单例：`OnceLock` 保证只 `open` 一次（重复 `init_tracing` 不会多起 worker、
/// 也不会重新打开同一批文件）。注意它**永不 drop**——进程退出前的排空依赖
/// [`shutdown`]，见 `telemetry::shutdown`。
static FILE: OnceLock<RotatingFile> = OnceLock::new();

/// 滚动文件写入器的共享句柄（可克隆，`fmt` 层与关闭路径各持一份）。
#[derive(Clone, Debug)]
pub(crate) struct RotatingFile {
    inner: Arc<Mutex<Writer>>,
}

impl RotatingFile {
    /// 按配置打开写入器（目录不存在会自动创建）。
    ///
    /// # Errors
    /// 配置无法映射到 rotate-rs、目录不可写、文件被占用等返回
    /// [`StartupError::Telemetry`]（启动期 fail-fast，不静默降级成"不落盘"）。
    fn open(cfg: &FileSection) -> Result<Self, StartupError> {
        let max_size = usize::try_from(cfg.max_size.as_u64()).map_err(|_| {
            StartupError::Telemetry(format!(
                "[telemetry].file.max_size = {} 超出当前平台的 usize 上限",
                cfg.max_size.as_u64()
            ))
        })?;
        let max_pending_bytes = usize::try_from(cfg.max_pending_bytes.as_u64()).map_err(|_| {
            StartupError::Telemetry(format!(
                "[telemetry].file.max_pending_bytes = {} 超出当前平台的 usize 上限",
                cfg.max_pending_bytes.as_u64()
            ))
        })?;

        let rotation = match cfg.rotation {
            FileRotation::Size => Rotation::Size(max_size),
            FileRotation::Time => Rotation::Time(cfg.interval),
            FileRotation::Mixed => Rotation::Hybrid {
                max_size,
                interval: cfg.interval,
            },
        };
        let mode = match cfg.mode {
            FileWriteMode::Sync => Mode::Sync,
            FileWriteMode::Nonblocking => Mode::NonBlocking(NonBlockingConfig {
                // 0 = 沿用库默认值（128K 条）；配置里不强迫用户记住这些数字
                channel_capacity: cfg.channel_capacity,
                max_pending_bytes,
                overflow: match cfg.overflow {
                    FileOverflow::Drop => OverflowStrategy::DropNew,
                    FileOverflow::Block => OverflowStrategy::Block,
                },
            }),
        };

        let config = Config {
            dir: cfg.dir.trim().to_string(),
            name: cfg.name.clone(),
            suffix: cfg.suffix.clone(),
            sep: NAME_SEP.to_string(),
            date_format: DATE_FORMAT.to_string(),
            template: TEMPLATE.to_string(),
            rotation,
            // `max_files = 0` 表示不清理归档
            max_backups: (cfg.max_files > 0).then_some(cfg.max_files as usize),
            compress: cfg.compress,
            mode,
            // 回调里禁止再打日志（rotate-rs 约束：Block 策略下会死锁、DropNew 下
            // 错误日志会挤占 channel），保持 None 用库自带的 stderr 节流输出。
            error_handler: None,
        };

        let writer = rotate_rs::open(config).map_err(|error| {
            StartupError::Telemetry(format!(
                "[telemetry].file 日志文件初始化失败（dir={} name={}）：{error}",
                cfg.dir, cfg.name
            ))
        })?;

        Ok(Self {
            inner: Arc::new(Mutex::new(writer)),
        })
    }

    /// 取锁（毒化时恢复内部值：临界区内只有 rotate-rs 自己的状态机）。
    fn locked(&self) -> MutexGuard<'_, Writer> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 写入一整条日志（一次 `write` = rotate-rs 的一个消息单元）。
    ///
    /// # Errors
    /// worker 已退出 / 磁盘写入失败时返回错误；fmt 层会丢弃它（tracing 没有
    /// 上报通道），文件侧的持续失败由 rotate-rs 的 stderr 节流兜底。
    pub(crate) fn write_line(&self, line: &[u8]) -> io::Result<()> {
        self.locked().write_all(line)
    }

    /// 排空写队列（同步模式恒立即返回）。
    pub(crate) fn flush(&self, timeout: Duration) -> io::Result<()> {
        self.locked().flush_with_timeout(timeout)
    }

    /// 运行期被丢弃的日志条数（仅 `overflow = "drop"` 会累积）。
    pub(crate) fn dropped(&self) -> u64 {
        self.locked().dropped_count()
    }

    /// 关闭后台 worker（排空 channel 后退出）。之后的写入会返回错误。
    fn close(&self) {
        self.locked().shutdown();
    }
}

/// 取得进程内唯一的文件写入器：首次按 `cfg` 打开，之后一律复用。
///
/// # Errors
/// 首次打开失败时返回 [`StartupError::Telemetry`]；重复初始化时复用已打开的句柄
/// （配置变更需要重启才能生效，见 `docs/08` §2.9）。
pub(crate) fn install(cfg: &FileSection) -> Result<RotatingFile, StartupError> {
    if let Some(existing) = FILE.get() {
        return Ok(existing.clone());
    }
    let opened = RotatingFile::open(cfg)?;
    // 并发 `init_tracing` 极少见；先到者胜，后到者复用（与 `FILTER` 同语义）
    let _ = FILE.set(opened);
    FILE.get()
        .cloned()
        .ok_or_else(|| StartupError::Telemetry("[telemetry].file 写入器初始化状态异常".to_string()))
}

/// 进程退出前排空并关闭文件日志（未启用 `log-file` 或未初始化时为空操作）。
///
/// 只在锁**之外**打日志：临界区内再触发一次 tracing 事件会重新进入
/// [`RotatingFile::write_line`] 并把互斥锁锁死。
pub(crate) fn shutdown(timeout: Duration) {
    let Some(file) = FILE.get() else {
        return;
    };
    let dropped = file.dropped();
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "[telemetry].file.overflow = \"drop\"：运行期有日志因队列满被丢弃"
        );
    }
    if let Err(error) = file.flush(timeout) {
        tracing::warn!(%error, "文件日志排空未完成：最后一段日志可能未落盘");
    }
    file.close();
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use byte_unit::Byte;

    use super::*;

    /// 测试用临时目录（按用例名区分，避免并行执行时互相覆盖）。
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurora-logfile-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 基础配置：同步写 + 不压缩，便于断言文件内容。
    fn file_cfg(dir: &Path) -> FileSection {
        FileSection {
            enabled: true,
            dir: dir.to_string_lossy().into_owned(),
            compress: false,
            max_files: 5,
            mode: FileWriteMode::Sync,
            ..FileSection::default()
        }
    }

    /// 归档文件名（排除活跃文件 `app.log`）。
    fn archives(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("读日志目录")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("app.") && name != "app.log")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn should_write_lines_to_active_file() {
        let dir = temp_dir("basic");
        let file = RotatingFile::open(&file_cfg(&dir)).expect("打开写入器");

        file.write_line(b"line-1\n").unwrap();
        file.write_line(b"line-2\n").unwrap();
        file.flush(Duration::from_secs(5)).unwrap();
        file.close();

        let content = std::fs::read_to_string(dir.join("app.log")).expect("活跃文件应存在");
        assert!(content.contains("line-1"), "实际：{content}");
        assert!(content.contains("line-2"), "实际：{content}");
        assert!(archives(&dir).is_empty(), "未到阈值不应产生归档");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_rotate_by_size() {
        let dir = temp_dir("size");
        let mut cfg = file_cfg(&dir);
        cfg.rotation = FileRotation::Size;
        cfg.max_size = Byte::from_u64(1024);
        let file = RotatingFile::open(&cfg).expect("打开写入器");

        for _ in 0..5 {
            file.write_line(&[b'x'; 800]).unwrap();
        }
        file.flush(Duration::from_secs(5)).unwrap();
        file.close();

        let rotated = archives(&dir);
        assert!(!rotated.is_empty(), "超过 max_size 必须产生归档");
        assert!(
            rotated.iter().all(|name| name.ends_with(".log")),
            "未开启压缩时归档应保留 .log：{rotated:?}"
        );
        assert!(dir.join("app.log").exists(), "活跃文件应被重建");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_compress_archived_file() {
        let dir = temp_dir("gz");
        let mut cfg = FileSection {
            mode: FileWriteMode::Nonblocking,
            ..file_cfg(&dir)
        };
        cfg.rotation = FileRotation::Size;
        cfg.max_size = Byte::from_u64(512);
        cfg.compress = true;
        let file = RotatingFile::open(&cfg).expect("打开写入器");

        for _ in 0..4 {
            file.write_line(&[b'y'; 600]).unwrap();
        }
        file.flush(Duration::from_secs(5)).unwrap();
        file.close();

        // 压缩由独立线程完成：关闭时 join 排空，理论上此处已就绪，
        // 仍留一个短轮询窗口避免平台调度差异导致的偶发失败。
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while archives(&dir).iter().all(|name| !name.ends_with(".gz")) {
            assert!(
                std::time::Instant::now() < deadline,
                "开启 compress 后应出现 .gz 归档，实际：{:?}",
                archives(&dir)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_keep_only_max_files_archives() {
        let dir = temp_dir("cleanup");
        let mut cfg = file_cfg(&dir);
        cfg.rotation = FileRotation::Size;
        cfg.max_size = Byte::from_u64(256);
        cfg.max_files = 2;
        let file = RotatingFile::open(&cfg).expect("打开写入器");

        for _ in 0..8 {
            file.write_line(&[b'z'; 300]).unwrap();
        }
        file.flush(Duration::from_secs(5)).unwrap();
        file.close();

        let rotated = archives(&dir);
        assert!(
            rotated.len() <= 2,
            "max_files = 2 时不应超过 2 个归档：{rotated:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_flush_non_blocking_writer() {
        let dir = temp_dir("nonblocking");
        let mut cfg = file_cfg(&dir);
        cfg.mode = FileWriteMode::Nonblocking;
        let file = RotatingFile::open(&cfg).expect("打开写入器");

        file.write_line(b"queued\n").unwrap();
        file.flush(Duration::from_secs(5)).unwrap();

        let content = std::fs::read_to_string(dir.join("app.log")).expect("活跃文件应存在");
        assert!(content.contains("queued"), "flush 后必须已落盘：{content}");
        assert_eq!(file.dropped(), 0);
        file.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_report_error_instead_of_panicking_when_dir_unusable() {
        let dir = temp_dir("baddir");
        std::fs::create_dir_all(&dir).unwrap();
        // 让日志目录的位置已经是一个文件：`create_dir_all` 必然失败
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();

        let mut cfg = file_cfg(&dir);
        cfg.dir = blocker.to_string_lossy().into_owned();
        let error = RotatingFile::open(&cfg).unwrap_err();
        assert!(
            error.to_string().contains("日志文件初始化失败"),
            "实际：{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
