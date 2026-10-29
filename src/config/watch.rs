//! 配置热更新（feature `watch`）。
//!
//! 流程：`文件变更 → 防抖 300ms → 用同一个 ConfigSource 重新加载 + 校验 → 原子替换
//! ConfigHandle（ArcSwap）→ 触发回调`。校验失败**保留旧值**（fail-safe），
//! 避免坏文件把服务打挂。
//!
//! # 监听什么由 `ConfigSource` 决定
//!
//! 框架不知道"配置目录"在哪：监听的是 [`ConfigSource::file_paths`] 里那些文件的
//! **父目录**。同一个 source 既用于启动加载、也用于热更新重放——不会出现
//! "启动读 A 文件、热更新读 B 文件"的两份真相。
//!
//! 重载走 [`ConfigSource::load`]（**含 `validate()`**），与启动路径完全一致；
//! 否则 `[tls].cert_dir` 不存在、`backend = "redis"` 却没填 url 这类坏配置会绕过
//! 启动护栏直接进入运行态。
//!
//! **生效范围**（`docs/08` §12.1）：只有**请求处理时重新 `state.config.load()`** 的值
//! 即时生效；启动期一次性装配的（中间件栈、CORS、监听地址、连接池）仍需重启。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::Watcher as _;

use crate::config::{AppConfig, ConfigHandle, ConfigSource};
use crate::errors::{AppError, AppResult, ErrorCode};

/// 防抖窗口：编辑器保存常产生多个事件（临时文件 + rename）。
const DEBOUNCE: Duration = Duration::from_millis(300);

/// 配置变更回调（替换成功后调用）。
type ChangeCallback = Arc<dyn Fn(&AppConfig) + Send + Sync>;

/// 配置热更新器；**drop 即停止监听**。
pub struct ConfigWatcher {
    task: tokio::task::JoinHandle<()>,
    handle: ConfigHandle,
    source: ConfigSource,
}

impl ConfigWatcher {
    /// 监听 `source` 声明的那些文件。
    ///
    /// # Errors
    /// `source` 未声明任何文件、或父目录不可监听时返回 `50005`。
    pub fn start(handle: ConfigHandle, source: ConfigSource) -> AppResult<Self> {
        Self::start_with(handle, source, None::<fn(&AppConfig)>)
    }

    /// 同 [`Self::start`]，并在每次成功替换后调用 `on_change`。
    ///
    /// 供「启动时自动开启热更新」使用（`[watch].enabled = true`）。
    ///
    /// # Errors
    /// 同 [`Self::start`]。
    pub fn start_with_callback<F>(
        handle: ConfigHandle,
        source: ConfigSource,
        on_change: F,
    ) -> AppResult<Self>
    where
        F: Fn(&AppConfig) + Send + Sync + 'static,
    {
        Self::start_with(handle, source, Some(on_change))
    }

    /// 监听 `source`，并在每次成功替换后调用 `on_change`。
    ///
    /// 回调适合用来**重建依赖配置的组件**（如重新设置日志级别）；回调在 watcher 任务里
    /// 串行执行，不要在其中做长时间阻塞操作。
    ///
    /// # Errors
    /// `source` 未声明任何文件、或父目录不可监听时返回 `50005`。
    pub fn start_with<F>(
        handle: ConfigHandle,
        source: ConfigSource,
        on_change: Option<F>,
    ) -> AppResult<Self>
    where
        F: Fn(&AppConfig) + Send + Sync + 'static,
    {
        let dirs = watch_dirs(&source);
        if dirs.is_empty() {
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "ConfigSource 未声明任何配置文件，无法热更新（配置只来自环境变量？）",
            ));
        }

        let callback: Option<ChangeCallback> = on_change.map(|f| Arc::new(f) as ChangeCallback);
        let watched = handle.clone();

        // **同步**创建并注册监听器：若放进 spawn 的任务里，`start_with` 会先返回 `Ok`，
        // 而「返回后立刻发生的变更」可能在注册完成前就发生 —— 事件永久丢失。
        // 同步注册还让「目录不可监听」在启动期暴露，而不是只留一行 warn。
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|error| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("配置监听器创建失败：{error}"),
            )
        })?;
        for dir in &dirs {
            watcher
                .watch(dir, notify::RecursiveMode::NonRecursive)
                .map_err(|error| {
                    AppError::biz(
                        ErrorCode::SERVICE_UNAVAILABLE,
                        format!("监听配置目录失败（{}）：{error}", dir.display()),
                    )
                })?;
        }
        tracing::info!(
            dirs = ?dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
            files = ?source.file_paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "配置热更新已启用"
        );

        let reload_source = source.clone();
        let task = tokio::spawn(async move {
            // watcher 必须活到任务结束，否则监听立即失效
            let _watcher = watcher;

            while rx.recv().await.is_some() {
                // 防抖：持续有事件就顺延，静默 300ms 后才真正重载
                loop {
                    tokio::select! {
                        () = tokio::time::sleep(DEBOUNCE) => break,
                        Some(_) = rx.recv() => {}
                    }
                }

                match reload_source.load() {
                    Ok(reloaded) => {
                        // 安全边界只可收紧：放宽会被拒绝，此时**保留旧配置**
                        if let Err(reason) = watched.store(reloaded.clone()) {
                            tracing::warn!(
                                %reason,
                                "配置热更新被拒绝（会放宽安全边界），保留旧配置；需重启生效"
                            );
                            continue;
                        }
                        tracing::info!("配置已热更新（仅对新请求生效）");
                        if let Some(callback) = &callback {
                            callback(&reloaded);
                        }
                    }
                    Err(error) => {
                        // fail-safe：坏配置绝不能让服务失去可用配置
                        tracing::warn!(%error, "配置热更新失败（加载或校验不通过），保留旧配置");
                    }
                }
            }
        });

        Ok(Self {
            task,
            handle,
            source,
        })
    }

    /// 立即重载一次（手动触发 / 测试用）。
    ///
    /// # Errors
    /// 加载或校验失败时返回错误，**旧配置保持不变**。
    pub fn reload_now(&self) -> AppResult<()> {
        let reloaded = self.source.load().map_err(|error| {
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                format!("配置重载失败：{error}"),
            )
        })?;
        self.handle
            .store(reloaded)
            .map_err(|reason| AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, reason))
    }

    /// 当前配置快照。
    #[must_use]
    pub fn current(&self) -> Arc<AppConfig> {
        self.handle.load()
    }

    /// 正在监听的目录。
    #[must_use]
    pub fn watched_dirs(&self) -> Vec<PathBuf> {
        watch_dirs(&self.source)
    }
}

/// 需要监听的目录：`source` 声明的文件的父目录（去重）。
///
/// 相对路径（如 `config/dev.toml`）的父目录是 `config`；没有父目录时退回 `.`。
fn watch_dirs(source: &ConfigSource) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in source.file_paths() {
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigWatcher")
            .field("finished", &self.task.is_finished())
            .field("files", &self.source.file_paths())
            .finish()
    }
}
