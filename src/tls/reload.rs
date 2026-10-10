//! 触发编排与监控：周期轮询、目录变更监听（仅目录模式）、证书到期探针。
//!
//! - **轮询 / 到期探针**：TLS 开启即生效，与目录模式无关；
//! - **目录监听**：仅当 `[server.tls].dir` 非空时启用（目录只作**变更触发器**，
//!   框架不读证书内容）。

use std::sync::Arc; // 引入 Arc，跨线程共享 TLS 状态
use std::time::Duration; // 引入时长类型

use crate::observability::health::{HealthCheck, HealthStatus}; // 引入健康探针契约与状态

use super::TlsState; // 引入 TLS 状态

/// 启动周期轮询任务：到点即调用 `reload()`，失败保留旧证书并告警（fail-safe）。
pub fn spawn_poller(state: Arc<TlsState>, interval_secs: u64) { // 启动轮询任务
    tokio::spawn(async move { // 后台任务
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1))); // 定时器（至少 1s）
        ticker.tick().await; // 首次立即返回：跳过（启动时已 reload 过）
        loop { // 循环轮询
            ticker.tick().await; // 等待下一个周期
            match state.reload().await { // 重新装载证书
                Ok(n) => tracing::debug!(count = n, "tls cert poll reloaded"), // 成功：调试日志
                Err(e) => { // 失败
                    tracing::error!(error = %e, "tls cert poll reload failed, keeping previous"); // 保留旧值并告警
                }
            }
        }
    });
}

/// 启动目录监听（仅目录模式）：目录内发生变更即触发一次 `reload()`。
/// 目录只作**变更触发器**，框架不读证书内容。目录不存在时告警并放弃监听（不阻断）。
pub fn spawn_dir_watcher(dir: &str, state: Arc<TlsState>, debounce_ms: u64) { // 启动目录监听
    use notify::Watcher as _; // 引入 notify 的 watch 方法

    let path = std::path::PathBuf::from(dir); // 目录路径
    if !path.is_dir() { // 目录不存在
        tracing::warn!(dir = %dir, "tls dir not found, dir watcher disabled"); // 告警（不阻断）
        return; // 放弃监听
    }

    let (tx, rx) = std::sync::mpsc::channel::<()>(); // 事件通道：回调线程发、监听线程收
    let mut watcher = match notify::recommended_watcher( // 创建文件系统监听器
        move |res: notify::Result<notify::Event>| { // 事件回调
            if res.is_ok() { // 事件解析成功
                let _ = tx.send(()); // 转发一个空信号（只关心「有变更」）
            }
        },
    ) {
        Ok(w) => w, // 创建成功
        Err(e) => { // 创建失败
            tracing::warn!(error = %e, "tls dir watcher init failed, dir watching disabled"); // 告警
            return; // 放弃监听
        }
    };
    if let Err(e) = watcher.watch(&path, notify::RecursiveMode::NonRecursive) { // 非递归监听目录
        tracing::warn!(dir = %dir, error = %e, "tls dir watch failed"); // 告警
        return; // 放弃监听
    }

    let rt = tokio::runtime::Handle::current(); // 捕获运行时句柄（供 std 线程回调 async reload）
    let debounce = Duration::from_millis(debounce_ms.max(50)); // 防抖窗口（至少 50ms）
    let spawned = std::thread::Builder::new() // 创建监听线程
        .name("tls-dir-watcher".to_string()) // 命名线程便于排查
        .spawn(move || { // 启动线程
            let _watcher = watcher; // 保活：drop 即停发事件
            loop { // 事件主循环
                if rx.recv().is_err() { // 阻塞等待首个事件
                    return; // 通道关闭（进程退出）
                }
                let mut deadline = std::time::Instant::now() + debounce; // 防抖截止时刻
                loop { // 防抖等待
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now()); // 剩余时长
                    if remaining.is_zero() { // 到达截止
                        break; // 结束等待
                    }
                    match rx.recv_timeout(remaining) { // 窗口内继续收事件
                        Ok(_) => deadline = std::time::Instant::now() + debounce, // 有新事件则顺延
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break, // 静默窗口结束
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return, // 通道断开
                    }
                }
                let st = state.clone(); // 克隆 TLS 状态移入异步任务
                rt.spawn(async move { // 在运行时上异步触发 reload
                    match st.reload().await { // 重新装载证书
                        Ok(n) => tracing::info!(count = n, "tls certs reloaded (dir change)"), // 成功日志
                        Err(e) => { // 失败
                            tracing::error!(error = %e, "tls reload failed on dir change, keeping previous"); // 保留旧值并告警
                        }
                    }
                });
            }
        });
    if spawned.is_err() { // 线程创建失败
        tracing::warn!("spawn tls-dir-watcher thread failed"); // 告警
        return; // 放弃
    }
    tracing::info!(dir = %dir, "tls dir watcher started"); // 记录已启动
}

/// 证书到期探针：注册到 `/ready`。任一证书**已过期** → Down；临近到期仅告警。
pub struct ExpiryProbe { // 到期探针
    state: Arc<TlsState>, // TLS 状态（读证书仓库）
    warn_days: i64, // 临期告警阈值（天）
}

impl ExpiryProbe { // 为探针实现构造
    pub fn new(state: Arc<TlsState>) -> Self { // 由 TLS 状态构造
        Self { state, warn_days: 30 } // 默认 30 天内告警
    }
}

#[async_trait::async_trait] // 让 trait 支持 async 方法
impl HealthCheck for ExpiryProbe { // 实现健康探针契约
    fn name(&self) -> &str { // 探针名称
        "tls_cert_expiry" // 作为 /ready 的 custom 字段键
    }

    async fn check(&self) -> HealthStatus { // 执行探测
        let store = self.state.store(); // 取证书仓库句柄
        let guard = store.load(); // 读当前仓库
        let now = chrono::Utc::now().timestamp(); // 当前时间戳
        let mut expired = false; // 是否已有证书过期
        for (domain, ts) in guard.not_after() { // 遍历证书到期时间
            let days = (*ts - now) / 86_400; // 剩余天数
            if days < 0 { // 已过期
                expired = true; // 标记过期
                tracing::error!(domain = %domain, "tls certificate EXPIRED"); // 错误日志
            } else if days < self.warn_days { // 临近到期
                tracing::warn!(domain = %domain, days = days, "tls certificate expiring soon"); // 告警日志
            }
        }
        if expired { HealthStatus::Down } else { HealthStatus::Up } // 有过期则 Down
    }
}
