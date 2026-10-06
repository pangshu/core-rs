//! `Settings` 根结构：集中聚合所有配置节（文档 三·4），以及
//! [`ConfigHandle`] —— 加载完成后对外**唯一**的只读句柄（`ArcSwap`，读取零锁）。

use std::sync::Arc;
use std::sync::Mutex;

use arc_swap::{ArcSwap, Guard};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::sections::*;

/// 框架配置根。应用在**自己侧**用 `#[serde(flatten)]` 追加业务节：
///
/// ```rust,ignore
/// #[derive(Deserialize)]
/// pub struct AppSettings {
///     #[serde(flatten)]
///     pub fw: core_rs::config::Settings,
///     pub app: BizSettings,
/// }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub server: ServerSettings,
    #[serde(default)]
    pub database: DatabaseSettings,
    #[serde(default)]
    pub cache: CacheSettings,
    #[serde(default)]
    pub queue: QueueSettings,
    #[serde(default)]
    pub realtime: RealtimeSettings,
    #[serde(default)]
    pub task: TaskSettings,
    #[serde(default)]
    pub resilience: ResilienceSettings,
    #[serde(default)]
    pub i18n: I18nSettings,
    #[serde(default)]
    pub log: LogSettings,
    #[serde(default)]
    pub auth: AuthSettings,
    #[serde(default)]
    pub authz: AuthzSettings,
}

impl Settings {
    /// 日志与追踪用的服务标识，回退 `log.service_name` → "core-rs"
    pub fn service_name(&self) -> &str {
        if !self.log.service_name.is_empty() {
            return &self.log.service_name;
        }
        "core-rs"
    }
}

/// OnChange 回调：拿到**新**配置快照。在监听线程同步执行，请保持轻量。
pub type OnChange<T> = Arc<dyn Fn(&T) + Send + Sync>;

/// 配置只读句柄（内部 `ArcSwap<T>`）：热更新原子替换，读方拿到的总是当前生效快照。
/// 应用需要业务节时，用自己的 `AppSettings` 另建一个 handle（同一个加载器）。
pub struct ConfigHandle<T: DeserializeOwned + Clone + Send + Sync + 'static> {
    inner: Arc<ArcSwap<T>>,
    // 回调表与 inner 同生命周期共享：clone 出来的句柄（Watcher / CoreState::clone）
    // 必须能看到同一份订阅者，否则热更新通知 100% 丢失
    callbacks: Arc<Mutex<Vec<OnChange<T>>>>,
}

impl<T: DeserializeOwned + Clone + Send + Sync + 'static> Clone for ConfigHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            callbacks: Arc::clone(&self.callbacks),
        }
    }
}

impl<T: DeserializeOwned + Clone + Send + Sync + 'static> ConfigHandle<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(value)),
            callbacks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 当前生效快照（持有期间配置热更新不会被观察者读到一半）
    pub fn load(&self) -> Guard<Arc<T>> {
        self.inner.load()
    }

    /// 无阻塞地取当前快照的 Arc
    pub fn load_full(&self) -> Arc<T> {
        self.inner.load_full()
    }

    /// 原子替换（热更新重载成功时调用；失败调用方**不应**调用本方法）。
    /// 先替换后通知：回调里 `handle.load()` 读到的是新值。
    pub fn store(&self, value: T) {
        let arc = Arc::new(value);
        self.inner.store(arc);
        self.notify();
    }

    /// 注册变更回调（重载成功后依次触发）
    pub fn subscribe(&self, cb: OnChange<T>) {
        self.callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(cb);
    }

    /// 通知回调：**锁外执行**（先克隆出列表再遍历）——回调内再 subscribe/store
    /// 不会死锁；单个回调 panic 不影响其他回调与后续通知。
    fn notify(&self) {
        let callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(); // MutexGuard 随本语句结束释放
        let value = self.inner.load_full();
        for cb in callbacks.iter() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(&value)));
        }
    }
}

impl<T: DeserializeOwned + Clone + Send + Sync + 'static> std::fmt::Debug for ConfigHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigHandle").finish_non_exhaustive()
    }
}
