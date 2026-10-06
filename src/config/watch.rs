//! 热更新：监听配置目录变更 → 重新合并校验 → `ArcSwap` 原子替换 → 通知订阅者。
//!
//! - 校验失败（toml 写坏、结构不匹配）**保留旧值并告警**（fail-safe，不中断服务）；
//! - 监听父目录并按文件名过滤（编辑器保存/原子替换都不会丢事件），去抖合并连发事件；
//! - 环境变量覆盖在每次重载时按原优先级重新应用；
//! - 重载只替换配置值：DB / Redis 连接池、JWT 密钥等已初始化资源**不会**自动重建，
//!   需要响应变更的组件通过 [`ConfigHandle::subscribe`] 注册回调自行处理
//!   （如日志级别即时生效）。

use std::time::Duration;

use notify::Watcher as _;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::settings::ConfigHandle;
use super::source::LoadOptions;

/// 热更新监听器（泛型配置根：框架用 `Settings`，应用可用 `AppSettings`）。
/// 变更通知统一走 [`ConfigHandle::subscribe`] 注册的回调。
pub struct Watcher<T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static> {
    options: LoadOptions,
    handle: ConfigHandle<T>,
    debounce: Duration,
}

impl<T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static> Watcher<T> {
    pub fn new(options: LoadOptions, handle: ConfigHandle<T>, debounce_ms: u64) -> Self {
        Self {
            options,
            handle,
            debounce: Duration::from_millis(debounce_ms.max(50)),
        }
    }

    /// 被监听的文件（default.toml + {env}.toml，存在的才监听）
    fn watched_files(&self) -> Vec<std::path::PathBuf> {
        ["default", self.options.environment.file_stem()]
            .iter()
            .map(|stem| std::path::Path::new(&self.options.dir).join(format!("{stem}.toml")))
            .filter(|p| p.is_file())
            .collect()
    }

    /// 启动监听线程。文件一个都不存在时直接返回（无事可监听）。
    pub fn spawn(self) {
        let files = self.watched_files();
        if files.is_empty() {
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel::<notify::Event>();
        let mut watcher = match notify::recommended_watcher(move |res| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "config watcher init failed, hot reload disabled");
                return;
            }
        };

        // 监听父目录 + 按文件名过滤：直接 watch 文件在编辑器 rename/替换后失效
        let watched_names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
            .collect();
        let mut watched_dirs: Vec<std::path::PathBuf> = Vec::new();
        for f in &files {
            let dir = f.parent().map(|d| d.to_path_buf()).unwrap_or_default();
            if !watched_dirs.contains(&dir) {
                if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
                    // 单目录失败只放弃该目录，不影响已成功监听的其他目录
                    tracing::warn!(dir = %dir.display(), error = %e, "watch dir failed, skipping");
                    continue;
                }
                watched_dirs.push(dir);
            }
        }

        let options = self.options.clone();
        let handle = self.handle;
        let debounce = self.debounce;
        std::thread::Builder::new()
            .name("config-watcher".to_string())
            .spawn(move || {
                let _watcher = watcher; // 保活：drop 即停发事件，监听线程随之退出
                loop {
                    let Ok(first) = rx.recv() else {
                        return; // channel 关闭（进程退出）
                    };
                    if !matches_watched(&first, &watched_names) {
                        continue;
                    }
                    // 防抖：等到静默窗口再重载（编辑器一次保存连发多个事件）。
                    // 窗口内的**无关文件**事件不得重置窗口，否则目录里其他文件
                    // （日志、.gitkeep）持续变动会无限推迟 reload。
                    let mut deadline =
                        std::time::Instant::now() + debounce;
                    loop {
                        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(remaining) {
                            Ok(ev) => {
                                if matches_watched(&ev, &watched_names) {
                                    deadline = std::time::Instant::now() + debounce;
                                }
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    reload_once(&options, &handle);
                }
            })
            .expect("spawn config-watcher thread failed");
    }
}

/// 事件是否指向被监听的配置文件（只关心内容变更类事件）
fn matches_watched(ev: &notify::Event, names: &[String]) -> bool {
    let relevant = matches!(
        ev.kind,
        notify::EventKind::Modify(_) | notify::EventKind::Create(_) | notify::EventKind::Remove(_)
    );
    relevant
        && ev.paths.iter().any(|p| {
            p.file_name()
                .map(|n| names.contains(&n.to_string_lossy().to_string()))
                .unwrap_or(false)
        })
}

/// 执行一次重载：成功则记录变更、原子切换（并触发 handle 上注册的回调）；
/// 失败保留旧配置并告警（fail-safe）
pub fn reload_once<T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static>(
    options: &LoadOptions,
    handle: &ConfigHandle<T>,
) {
    match super::source::load::<T>(options) {
        Ok(new) => {
            log_changes(handle.load().as_ref(), &new);
            handle.store(new);
        }
        Err(e) => {
            tracing::error!(error = %e, "config reload failed, keeping previous config");
        }
    }
}

/// 顶层段级 diff 日志：info 列出变化的段名，debug 输出新配置全文
fn log_changes<T: serde::Serialize>(old: &T, new: &T) {
    let (Ok(old_v), Ok(new_v)) = (serde_json::to_value(old), serde_json::to_value(new)) else {
        return;
    };
    let (Some(old_obj), Some(new_obj)) = (old_v.as_object(), new_v.as_object()) else {
        return;
    };
    let changed: Vec<&String> = old_obj
        .keys()
        .filter(|k| old_obj.get(*k) != new_obj.get(*k))
        .chain(new_obj.keys().filter(|k| !old_obj.contains_key(*k)))
        .collect();
    if changed.is_empty() {
        return;
    }
    let names: Vec<&str> = changed.iter().map(|s| s.as_str()).collect();
    // 只打变更段名：全文 JSON 会把 DB 密码 / JWT secret / 各连接串打进日志
    //（环境变量来源同样会被合并进 Settings），debug 级别也不例外
    tracing::info!(changed = ?names, "config hot-reloaded");
}
