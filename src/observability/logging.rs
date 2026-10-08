//! tracing 初始化（文档 三·8）：**分级**（`RUST_LOG` > `[log].level`）、
//! **结构化输出**（console / JSON，JSON 便于采集进 ELK / Loki）、
//! 输出目标 stdout +（feature = "log-file"）滚动文件可并存。
//! feature = "otel" 时将 OpenTelemetry layer 一并挂到订阅链上。
//!
//! 进程内重复 init 安全：全局订阅链只在首次安装（后续 `set_global_default`
//! 失败即忽略），滚动文件写入器亦经进程级单例只构造一次、重复装配复用同一份，
//! 测试多次装配不受影响。

use std::io::Write; // 引入 Write trait，实现自定义日志写入器

use tracing_subscriber::{prelude::*, registry, EnvFilter}; // 引入订阅链组合、registry 与日志级别过滤器

use crate::config::Settings; // 引入全局配置类型

/// 输出目的地（console / file 共用同一 fmt layer 类型，便于链式组合）：
/// - `Stdout`：控制台输出（保持 ANSI 着色）；
/// - `File`：rotate-rs 滚动文件（关闭 ANSI）；
/// - `Sink`：黑洞（对应层未启用时的占位，写入即丢弃）。
enum LogSink { // 日志输出目的地枚举
    Stdout, // 控制台
    #[cfg(feature = "log-file")] // 仅在开启 log-file 时存在该变体
    File(file_writer::SharedRotatingWriter), // 滚动文件写入器
    Sink, // 黑洞占位
}

impl LogSink {
    fn stdout() -> Self { // 构造控制台目的地
        Self::Stdout // 返回 Stdout 变体
    }

    #[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
    fn file_or_sink(w: Option<file_writer::SharedRotatingWriter>) -> Self { // 有写入器用 File，否则用 Sink
        match w { // 依据可选写入器分派
            Some(w) => Self::File(w), // 有写入器
            None => Self::Sink, // 无写入器则黑洞
        }
    }
}

struct SinkWriter<'a>(&'a LogSink); // 把 LogSink 适配为 fmt 所需的 Writer

impl Write for SinkWriter<'_> { // 为适配器实现 Write
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { // 写入字节
        match self.0 { // 按目的地分派
            LogSink::Stdout => std::io::stdout().write_all(buf).map(|_| buf.len()), // 写标准输出并返回写入长度
            LogSink::Sink => Ok(buf.len()), // 黑洞：直接丢弃但报成功
            #[cfg(feature = "log-file")] // 仅在开启 log-file 时编译该分支
            LogSink::File(w) => w.locked().write(buf), // 加锁后写入滚动文件
        }
    }

    fn flush(&mut self) -> std::io::Result<()> { // 刷出缓冲
        match self.0 { // 按目的地分派
            LogSink::Stdout => std::io::stdout().flush(), // 刷标准输出
            LogSink::Sink => Ok(()), // 黑洞：无需刷
            #[cfg(feature = "log-file")] // 仅在开启 log-file 时编译该分支
            LogSink::File(w) => w.locked().flush(), // 加锁后刷滚动文件
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink { // 让 LogSink 可作为 fmt 的写入器工厂
    type Writer = SinkWriter<'a>; // 产出的写入器类型

    fn make_writer(&'a self) -> Self::Writer { // 构造写入器
        SinkWriter(self) // 包装自身
    }
}

/// 日志后端保活句柄（持有到进程结束；Drop 时 flush 各后端缓冲）
pub struct LogGuard { // 日志保活句柄
    #[cfg(feature = "otel")] // 仅在开启 otel 时存在该字段
    _otel: Option<crate::observability::tracing::OtelGuard>, // OTel guard 占位（实际由静态保活）
    /// log-file feature：保活 rotate-rs writer（存活即后台 worker 存活）。
    /// `Box<dyn Any>` 仅用于抹平 feature 组合的类型差异。
    _file: Option<Box<dyn std::any::Any + Send + Sync>>, // 文件写入器保活句柄
}

#[cfg(feature = "log-file")] // 仅在开启 log-file 时编译该模块
mod file_writer { // rotate-rs 写入器的共享封装
    //! rotate-rs `Writer` → 共享句柄。Writer 的 `write` 需要 `&mut self`，
    //! 跨线程共享用 Mutex 包一层（NonBlocking 模式下 write 只是入队，锁竞争可忽略）。

    use std::sync::{Arc, Mutex, MutexGuard, PoisonError}; // 引入共享指针、互斥锁及其守卫与毒化错误

    /// 共享句柄：fmt layer 与 [`LogGuard`](super::LogGuard) 各持一份，
    /// 全部 Drop 后 rotate-rs worker 排空退出
    #[derive(Clone)] // 可克隆以便多处持有
    pub struct SharedRotatingWriter(pub(crate) Arc<Mutex<rotate_rs::Writer>>); // 加锁共享的滚动写入器

    impl SharedRotatingWriter {
        /// 毒化恢复：临界区内只有 rotate-rs 内部逻辑，无用户代码可 panic
        pub(crate) fn locked(&self) -> MutexGuard<'_, rotate_rs::Writer> { // 取锁并容忍毒化
            self.0.lock().unwrap_or_else(PoisonError::into_inner) // 毒化时取出内部值
        }
    }
}

/// 由 `[log.file]` 配置构造 rotate-rs 滚动写入器（进程内只调用一次）
#[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
fn build_rotating_writer( // 依据配置构造滚动写入器
    cfg: &crate::config::sections::FileLogSettings, // 文件日志配置
) -> file_writer::SharedRotatingWriter { // 返回共享写入器
    use std::time::Duration; // 引入时长类型

    let max_size = (cfg.max_size_mb.max(1) as usize).saturating_mul(1024 * 1024); // MB 转字节，至少 1MB 且防溢出
    let interval = Duration::from_secs(cfg.interval_secs.max(1)); // 时间轮转间隔，至少 1 秒
    let rotation = match cfg.rotation.as_str() { // 依据配置选择轮转策略
        "size" => rotate_rs::Rotation::Size(max_size), // 按大小轮转
        "time" => rotate_rs::Rotation::Time(interval), // 按时间轮转
        _ => rotate_rs::Rotation::Hybrid { max_size, interval }, // 默认大小+时间混合
    };
    let mode = if cfg.non_blocking { // 非阻塞模式配置
        rotate_rs::Mode::NonBlocking(rotate_rs::NonBlockingConfig { // 构造非阻塞配置
            // 0 = 使用 rotate-rs 默认容量
            channel_capacity: 0, // 通道容量用默认值
            max_pending_bytes: 0, // 待写字节上限用默认值
            overflow: if cfg.overflow == "drop" { // 溢出策略
                rotate_rs::OverflowStrategy::DropNew // 丢弃新日志
            } else {
                rotate_rs::OverflowStrategy::Block // 阻塞等待
            },
        })
    } else {
        rotate_rs::Mode::Sync // 同步模式
    };

    let rcfg = rotate_rs::Config { // 组装 rotate-rs 配置
        dir: cfg.dir.clone(), // 日志目录
        name: cfg.name.clone(), // 文件名前缀
        suffix: "log".to_string(), // 文件后缀
        sep: ".".to_string(), // 文件名各段分隔符
        // rotate-rs 要求时间串不含字母/路径分隔符（文件名可解析性约束）
        date_format: "%Y-%m-%d-%H-%M-%S".to_string(), // 时间串格式
        template: "{dir}/{name}{sep}{date}{sep}{counter}{sep}{suffix}".to_string(), // 文件名模板
        rotation, // 轮转策略
        max_backups: if cfg.max_backups == 0 { // 备份保留数
            None // 0 表示不限制
        } else {
            Some(cfg.max_backups) // 保留指定数量
        },
        compress: cfg.compress, // 是否压缩历史文件
        mode, // 同步/非阻塞模式
        // rotate-rs 自带 stderr 节流输出；回调里不能打日志（递归），保持 None
        error_handler: None, // 不注入错误回调，避免日志递归
    };

    match rotate_rs::open(rcfg) { // 打开写入器
        Ok(w) => file_writer::SharedRotatingWriter(std::sync::Arc::new(std::sync::Mutex::new(w))), // 成功则加锁共享
        // 启动期基础设施故障：fail-fast
        Err(e) => panic!( // 打开失败直接 panic
            "log file init failed (dir={}, name={}): {e}", // panic 消息模板
            cfg.dir, cfg.name // 出错目录与文件名
        ),
    }
}

/// 进程级单例槽：滚动写入器只构造一次，重复 init 复用同一份
/// （否则每次 init 都会重新 `rotate_rs::open` 同一批日志文件、多起一个后台 worker）
#[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
static FILE_WRITER: std::sync::OnceLock<file_writer::SharedRotatingWriter> = // 进程级一次性单例
    std::sync::OnceLock::new(); // 初始化为空

/// 取得进程内唯一的滚动写入器：首次按 `cfg` 构造，之后一律复用（真正幂等）
#[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
fn shared_file_writer( // 返回进程内共享的滚动写入器
    cfg: &crate::config::sections::FileLogSettings, // 文件日志配置（仅首次生效）
) -> file_writer::SharedRotatingWriter { // 返回共享写入器
    FILE_WRITER.get_or_init(|| build_rotating_writer(cfg)).clone() // 首次构造，之后克隆复用
}

/// 文件输出开关：`RUST_LOG_FILE=1` 环境变量或 `[log.file].enabled`
#[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
fn file_enabled(cfg: &Settings) -> bool { // 判断是否启用文件日志
    if std::env::var("RUST_LOG_FILE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) { // 环境变量显式开启
        return true; // 环境变量优先
    }
    cfg.log.file.enabled // 否则看配置项
}

/// 初始化全局 tracing。重复调用安全：全局订阅链只在首次安装，滚动文件写入器经
/// 进程级单例复用，后续调用不再产生新的副作用（等价 no-op，返回占位 guard）。
pub fn init(cfg: &Settings) -> LogGuard { // 初始化日志系统
    let filter = EnvFilter::try_from_default_env() // 优先用 RUST_LOG 环境变量
        .or_else(|_| EnvFilter::try_new(&cfg.log.level)) // 否则用配置的日志级别
        .unwrap_or_else(|_| EnvFilter::new("info")); // 都失败则回落 info

    #[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
    let shared_file: Option<file_writer::SharedRotatingWriter> = if file_enabled(cfg) { // 文件日志启用时构造写入器
        let w = shared_file_writer(&cfg.log.file); // 复用进程内唯一滚动写入器（只构造一次）
        tracing::info!( // 记录文件轮转已启用
            dir = %cfg.log.file.dir, // 日志目录
            name = %cfg.log.file.name, // 文件名
            "log file rotation enabled" // 事件消息
        );
        Some(w) // 返回写入器
    } else {
        None // 未启用则为 None
    };

    let file_keepalive: Option<Box<dyn std::any::Any + Send + Sync>>; // 文件写入器保活槽
    #[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
    { // log-file 构建下的赋值
        file_keepalive = shared_file.as_ref().map(|w| Box::new(w.0.clone()) as _); // 克隆共享句柄装箱保活
    }
    #[cfg(not(feature = "log-file"))] // 未开启 log-file 时编译
    { // 非 log-file 构建下的赋值
        file_keepalive = None; // 无文件写入器
    }

    let console_sink = LogSink::stdout(); // 控制台输出目的地
    #[cfg(feature = "log-file")] // 仅在开启 log-file 时编译
    let file_sink = LogSink::file_or_sink(shared_file.clone()); // 文件输出目的地（有则文件，无则黑洞）
    #[cfg(not(feature = "log-file"))] // 未开启 log-file 时编译
    let file_sink = LogSink::Sink; // 恒为黑洞

    // 按格式分派：同一分支内 console / file 两个 fmt layer 具体类型一致，可直接
    // 链式组合（fmt::Layer 的 Layer 实现绑定构造时的 Subscriber，无法跨格式混搭）
    match cfg.log.format.as_str() { // 依据输出格式分派
        "json" => { // JSON 结构化输出
            let console = cfg // 控制台层（可选）
                .log // 日志配置
                .stdout // 是否输出到 stdout
                .then(|| tracing_subscriber::fmt::layer().json().with_writer(console_sink)); // 开启时构造 JSON 控制台层
            let file = tracing_subscriber::fmt::layer() // 文件层
                .json() // JSON 格式
                .with_ansi(false) // 关闭 ANSI 着色
                .with_writer(file_sink); // 写入文件目的地
            mount(cfg, registry().with(console).with(file), filter) // 组装订阅链并挂载
        }
        _ => { // 默认（人类可读）格式
            let console = cfg // 控制台层（可选）
                .log // 日志配置
                .stdout // 是否输出到 stdout
                .then(|| tracing_subscriber::fmt::layer().with_writer(console_sink)); // 开启时构造默认格式控制台层
            let file = tracing_subscriber::fmt::layer() // 文件层
                .with_ansi(false) // 关闭 ANSI 着色
                .with_writer(file_sink); // 写入文件目的地
            mount(cfg, registry().with(console).with(file), filter) // 组装订阅链并挂载
        }
    };

    LogGuard { // 返回保活句柄
        #[cfg(feature = "otel")] // 仅在开启 otel 时初始化该字段
        _otel: None, // otel guard 由 GLOBAL_OTEL_GUARD 静态保活
        _file: file_keepalive, // 保活文件写入器
    }
}

/// 挂载 otel 层与过滤器；已初始化时 no-op
#[allow(unused_variables)] // cfg 参数：无 otel feature 时用不到
fn mount<S>( // 组装并安装全局订阅链
    cfg: &Settings, // 全局配置
    subscriber: S, // 已组装的订阅链
    filter: EnvFilter, // 级别过滤器
) -> tracing::Dispatch // 返回已安装的 Dispatch
where // 泛型约束
    S: tracing::Subscriber // 必须是订阅者
        + Send // 可跨线程发送
        + Sync // 可跨线程共享
        + 'static // 需为静态生命周期
        + for<'a> tracing_subscriber::registry::LookupSpan<'a>, // 且支持 span 查找
{ // 函数体开始
    #[cfg(feature = "otel")] // 仅在开启 otel 时编译
    { // otel 构建分支
        let service_name = if cfg.log.service_name.is_empty() { // 服务名未配置时
            "core-rs".to_string() // 回落默认名
        } else {
            cfg.log.service_name.clone() // 使用配置的服务名
        };
        match crate::observability::tracing::setup_layer::<S>(&service_name) { // 尝试构造 OTel layer
            Some((otel_layer, guard)) => { // 构造成功
                let dispatch = tracing::Dispatch::new(subscriber.with(otel_layer).with(filter)); // 挂上 otel 层与过滤器
                if tracing::dispatcher::set_global_default(dispatch.clone()).is_err() { // 全局默认已被占用（重复 init）
                    return tracing::dispatcher::get_default(|d| d.clone()); // 返回现有默认，等价 no-op
                }
                GLOBAL_OTEL_GUARD.set(guard).ok(); // 静态保活 otel guard
                return dispatch; // 返回已安装的 Dispatch
            }
            None => { // 构造失败（未配置端点等）
                let dispatch = tracing::Dispatch::new(subscriber.with(filter)); // 仅挂过滤器，降级纯本地
                if tracing::dispatcher::set_global_default(dispatch.clone()).is_err() { // 全局默认已被占用
                    return tracing::dispatcher::get_default(|d| d.clone()); // 返回现有默认
                }
                return dispatch; // 返回已安装的 Dispatch
            }
        }
    }

    #[cfg(not(feature = "otel"))] // 未开启 otel 时编译
    { // 非 otel 构建分支
        let dispatch = tracing::Dispatch::new(subscriber.with(filter)); // 仅挂过滤器
        if tracing::dispatcher::set_global_default(dispatch.clone()).is_err() { // 全局默认已被占用
            return tracing::dispatcher::get_default(|d| d.clone()); // 返回现有默认，等价 no-op
        }
        dispatch // 返回已安装的 Dispatch
    }
}

/// otel guard 保活（进程级，Drop 时 flush spans）
#[cfg(feature = "otel")] // 仅在开启 otel 时定义
static GLOBAL_OTEL_GUARD: std::sync::OnceLock<crate::observability::tracing::OtelGuard> = // 进程级一次性静态保活
    std::sync::OnceLock::new(); // 初始化为空
