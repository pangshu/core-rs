//! `Settings` 根结构：集中聚合所有配置节（文档 三·4），以及
//! [`ConfigHandle`] —— 加载完成后对外**唯一**的只读句柄（`ArcSwap`，读取零锁）。

use std::sync::Arc; // 引入原子引用计数指针，用于多线程共享配置
use std::sync::Mutex; // 引入互斥锁，保护回调订阅者列表

use arc_swap::{ArcSwap, Guard}; // 引入 ArcSwap 无锁原子替换句柄及其读守卫
use serde::de::DeserializeOwned; // 引入可反序列化 trait 作为泛型约束
use serde::{Deserialize, Serialize}; // 引入 serde 反序列化/序列化派生宏

use crate::config::sections::*; // 引入全部配置节结构用于聚合

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)] // 派生调试/克隆/默认与 serde 能力
pub struct Settings { // 框架配置根结构，聚合所有子系统配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub server: ServerSettings, // HTTP 服务配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub database: DatabaseSettings, // 数据库配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub cache: CacheSettings, // 缓存配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub queue: QueueSettings, // 消息队列配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub realtime: RealtimeSettings, // 实时通信配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub task: TaskSettings, // 定时任务配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub resilience: ResilienceSettings, // 弹性（熔断/重试/降级）配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub i18n: I18nSettings, // 国际化配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub log: LogSettings, // 日志配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub auth: AuthSettings, // 认证配置节
    #[serde(default)] // 缺失该节时用默认值填充
    pub authz: AuthzSettings, // 授权配置节
}

impl Settings { // 为配置根实现便捷方法
    /// 日志与追踪用的服务标识，回退 `log.service_name` → "core-rs"
    pub fn service_name(&self) -> &str { // 返回服务名，供日志/追踪标识
        if !self.log.service_name.is_empty() { // 若日志配置里显式指定了服务名
            return &self.log.service_name; // 优先使用配置的服务名
        }
        "core-rs" // 未配置时回落默认服务名
    }
}

/// OnChange 回调：拿到**新**配置快照。在监听线程同步执行，请保持轻量。
pub type OnChange<T> = Arc<dyn Fn(&T) + Send + Sync>; // 变更回调类型：线程安全的新配置快照闭包

/// 配置只读句柄（内部 `ArcSwap<T>`）：热更新原子替换，读方拿到的总是当前生效快照。
/// 应用需要业务节时，用自己的 `AppSettings` 另建一个 handle（同一个加载器）。
pub struct ConfigHandle<T: DeserializeOwned + Clone + Send + Sync + 'static> { // 泛型配置只读句柄
    inner: Arc<ArcSwap<T>>, // 内部用 ArcSwap 承载当前生效配置，读取零锁
    // 回调表与 inner 同生命周期共享：clone 出来的句柄（Watcher / CoreState::clone）
    // 必须能看到同一份订阅者，否则热更新通知 100% 丢失
    callbacks: Arc<Mutex<Vec<OnChange<T>>>>, // 共享的回调订阅者列表，保证克隆句柄可见同一份
}

impl<T: DeserializeOwned + Clone + Send + Sync + 'static> Clone for ConfigHandle<T> { // 手写 Clone 保证共享同一份底层数据
    fn clone(&self) -> Self { // 实现克隆
        Self { // 构造新的句柄
            inner: Arc::clone(&self.inner), // 克隆 Arc 指针，与原句柄共享同一份配置
            callbacks: Arc::clone(&self.callbacks), // 克隆 Arc 指针，与原句柄共享同一份回调表
        }
    }
}

impl<T: DeserializeOwned + Clone + Send + Sync + 'static> ConfigHandle<T> { // 为配置句柄实现核心方法
    pub fn new(value: T) -> Self { // 用初始配置值构造句柄
        Self { // 构造句柄
            inner: Arc::new(ArcSwap::from_pointee(value)), // 用初始值初始化 ArcSwap
            callbacks: Arc::new(Mutex::new(Vec::new())), // 初始化空的回调订阅者列表
        }
    }

    /// 当前生效快照（持有期间配置热更新不会被观察者读到一半）
    pub fn load(&self) -> Guard<Arc<T>> { // 取当前快照的读守卫
        self.inner.load() // 通过 ArcSwap 无锁读取当前配置
    }

    /// 无阻塞地取当前快照的 Arc
    pub fn load_full(&self) -> Arc<T> { // 取当前快照的 Arc（可长期持有）
        self.inner.load_full() // 通过 ArcSwap 克隆出当前配置的 Arc
    }

    /// 原子替换（热更新重载成功时调用；失败调用方**不应**调用本方法）。
    /// 先替换后通知：回调里 `handle.load()` 读到的是新值。
    pub fn store(&self, value: T) { // 原子替换为新配置并通知订阅者
        let arc = Arc::new(value); // 将新配置包成 Arc
        self.inner.store(arc); // 原子替换 ArcSwap 内的当前配置
        self.notify(); // 替换完成后触发所有变更回调
    }

    /// 注册变更回调（重载成功后依次触发）
    pub fn subscribe(&self, cb: OnChange<T>) { // 注册一个配置变更回调
        self.callbacks // 访问回调列表
            .lock() // 获取互斥锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
            .push(cb); // 把回调追加到订阅者列表
    }

    /// 通知回调：**锁外执行**（先克隆出列表再遍历）——回调内再 subscribe/store
    /// 不会死锁；单个回调 panic 不影响其他回调与后续通知。
    fn notify(&self) { // 依次触发所有已注册的变更回调
        let callbacks = self // 访问回调列表
            .callbacks // 回调表字段
            .lock() // 获取互斥锁
            .unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值
            .clone(); // MutexGuard 随本语句结束释放
        let value = self.inner.load_full(); // 取出当前（新）配置快照供回调使用
        for cb in callbacks.iter() { // 遍历回调列表逐个触发
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(&value))); // 捕获单个回调 panic，避免影响其他回调
        }
    }
}

impl<T: DeserializeOwned + Clone + Send + Sync + 'static> std::fmt::Debug for ConfigHandle<T> { // 手写 Debug 避免输出配置内容
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { // 实现格式化方法
        f.debug_struct("ConfigHandle").finish_non_exhaustive() // 只输出类型名，隐藏内部字段
    }
}
