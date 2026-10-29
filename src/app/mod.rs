//! 应用装配模块。
//!
//! 提供全局应用状态 [`AppState`]、装配逻辑与启动/关闭生命周期钩子。

mod builder;
mod lifecycle;
pub(crate) mod state;

pub use lifecycle::{
    DEFAULT_HOOK_TIMEOUT, FnShutdown, FnStartup, HookFailure, LifecycleError, LifecycleHook,
    LifecycleHooks, LifecycleReport,
};
pub use state::AppState;
