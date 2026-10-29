//! 启动与优雅关闭模块。
//!
//! 提供 [`serve`]（绑定监听 + 启动 + 优雅关闭）、[`serve_with_shutdown`] /
//! [`serve_listener_with_shutdown`]（可注入关闭信号，便于测试）、
//! [`shutdown_signal`] 与启动错误 [`StartupError`]。

mod serve;
mod shutdown;
pub(crate) mod startup;

#[cfg(feature = "serve-hardened")]
mod hardened;

pub use serve::{serve, serve_listener_with_shutdown, serve_with_shutdown};
pub use shutdown::shutdown_signal;
pub use startup::StartupError;
