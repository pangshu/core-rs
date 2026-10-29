//! 业务 feature（每个 feature 一个文件，内部按 handler / service / repo 组织）。
//!
//! 这里为了示例紧凑，handler 直接访问 `state.db` / `state.cache` / `state.queue`；
//! 真实项目建议按 `docs/13` §3 垂直切分出 service 与 repository 层。

pub mod auth;
pub mod todos;
