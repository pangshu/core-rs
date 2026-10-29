//! 限流中间件 re-export 壳（feature `ratelimit`）。
//!
//! 实现在顶层 `ratelimit/` 模块；本文件仅为 `pub(crate) use` 壳，便于按中间件目录
//! 组织（见 `docs/03` §4.6）。

pub(crate) use crate::ratelimit::layer::handle;
