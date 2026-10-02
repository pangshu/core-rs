//! 配置热更新（feature = "watch"）：监听 `app.yml`（及 `app-{profile}.yml`）变更，
//! 自动重载并原子切换 [`AppConfig`]，随后触发 OnChange 回调。
//!
//! - 重载失败（yml 写坏、语法错误）保留旧配置并记 error 日志，服务不受影响；
//! - 监听父目录并按文件名过滤（编辑器保存/原子替换都不会丢事件），300ms 防抖
//!   合并连发事件（VSCode 一次保存触发多个事件）；
//! - 环境变量覆盖在每次重载时按原优先级重新应用；
//! - 重载只替换配置值：DB/Redis 连接池、JWT 密钥等已初始化资源**不会**自动重建
//!   （避免生产连接被意外替换），业务需要响应变更时注册 OnChange 回调自行处理。
//!
//! ```no_run
//! # use core_rs::prelude::*;
//! # async fn demo() -> Result<(), AppError> {
//! Application::builder()
//!     .on_config_change(|cfg| {
//!         tracing::info!(level = %cfg.log.level, "config reloaded");
//!     })
//!     .run().await?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use notify::Watcher as _;

use crate::config::AppConfig;

/// OnChange 回调：拿到**新**配置快照。要求快速同步执行（在监听线程上运行，
/// 不要做阻塞 IO / 长任务，复杂处理请 spawn 到业务运行时）。
pub type OnChange = Arc<dyn Fn(&AppConfig) + Send + Sync>;

/// 重载所需的上下文（文件路径、profile、配置句柄、回调列表）
pub struct Watcher {
    path: String,
    profile: Option<String>,
    handle: Arc<ArcSwap<AppConfig>>,
    callbacks: Vec<OnChange>,
}

impl Watcher {
    pub fn new(
        path: impl Into<String>,
        profile: Option<String>,
        handle: Arc<ArcSwap<AppConfig>>,
        callbacks: Vec<OnChange>,
    ) -> Self {
        Self {
            path: path.into(),
            profile,
            handle,
            callbacks,
        }
    }

    /// 需要监听的文件（app.yml + 存在的 profile 覆盖文件）
    fn watched_files(&self) -> Vec<std::path::PathBuf> {
        let mut files = vec![std::path::PathBuf::from(&self.path)];
        if let Some(p) = &self.profile {
            files.push(std::path::PathBuf::from(profile_file(&self.path, p)));
        }
        files
            .into_iter()
            .filter(|p| p.is_file())
            .collect()
    }

    /// 启动监听线程（阻塞在该线程，直到进程退出）。文件一个都不存在时直接返回。
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
                    tracing::warn!(dir = %dir.display(), error = %e, "watch dir failed");
                    return;
                }
                watched_dirs.push(dir);
            }
        }

        let path = self.path.clone();
        let profile = self.profile.clone();
        std::thread::Builder::new()
            .name("config-watcher".to_string())
            .spawn(move || {
                let _watcher = watcher; // 保活：drop 即停发事件，监听线程随之退出
                loop {
                    // 首事件触发
                    let Ok(first) = rx.recv() else {
                        return; // channel 关闭（进程退出）
                    };
                    if !matches_watched(&first, &watched_names) {
                        continue;
                    }
                    // 防抖：等到 300ms 无新事件再重载（编辑器一次保存连发多个事件）
                    loop {
                        match rx.recv_timeout(Duration::from_millis(300)) {
                            Ok(_) => continue,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    reload_once(&path, profile.as_deref(), &self.handle, &self.callbacks);
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

/// 执行一次重载：成功则记录变更、原子切换、触发回调；失败保留旧配置
pub fn reload_once(
    path: &str,
    profile: Option<&str>,
    handle: &Arc<ArcSwap<AppConfig>>,
    callbacks: &[OnChange],
) {
    match AppConfig::load_with_profile(Some(path), profile) {
        Ok(new) => {
            log_changes(handle.load().as_ref(), &new);
            handle.store(Arc::new(new.clone()));
            for cb in callbacks {
                cb(&new);
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "config reload failed, keeping previous config");
        }
    }
}

/// 顶层段级 diff 日志：info 列出变化的段名，debug 输出新配置全文
fn log_changes(old: &AppConfig, new: &AppConfig) {
    let (Ok(old_v), Ok(new_v)) = (
        serde_json::to_value(old),
        serde_json::to_value(new),
    ) else {
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
    tracing::info!(changed = ?names, "config hot-reloaded");
    tracing::debug!(config = %new_v, "reloaded config content");
}

/// profile 文件名推导（与 config::mod 内部规则一致）
fn profile_file(path: &str, profile: &str) -> String {
    let p = std::path::Path::new(path);
    match (p.file_stem(), p.extension()) {
        (Some(stem), Some(ext)) => p
            .with_file_name(format!(
                "{}-{}.{}",
                stem.to_string_lossy(),
                profile,
                ext.to_string_lossy()
            ))
            .to_string_lossy()
            .to_string(),
        (Some(stem), None) => p
            .with_file_name(format!("{}-{}", stem.to_string_lossy(), profile))
            .to_string_lossy()
            .to_string(),
        _ => format!("{path}-{profile}"),
    }
}
