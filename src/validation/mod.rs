//! 输入校验（feature `security`）。
//!
//! - [`validate`] / [`validate_with`]：对任意 `validator::Validate` 执行校验，
//!   失败映射为 `10001`（400）并带 `details`。
//! - [`Validated<T>`] / [`ValidatedQuery<T>`] / [`ValidatedPath<T>`] /
//!   [`ValidatedForm<T>`]：提取器，先反序列化再校验，失败同样返回统一错误体。
//!
//! 字段明细的映射规则见 `rules`，提取器实现见 `extractor`，入口见 `validate`，
//! 配置投影见 [`options`]（其余为私有子模块）。配置见 `docs/08` §3 `[validation]`；
//! **不做业务规则编排**（见 `docs/11` §5）。
//!
//! # 边界（务必先读）
//!
//! 1. **校验 ≠ 清洗**：不做 XSS / SQL 注入过滤，参数化查询与输出编码由应用负责。
//! 2. **`Option<T>` 缺字段 = 跳过校验**（`validator` 语义）：PATCH 请求省略
//!    `email` 就不会跑邮箱格式校验，别把「可选」当成「免校验」。
//! 3. **业务规则不在这里**：唯一性、归属（「只能改自己的」）、状态机合法性
//!    必须由 handler 二次校验——这是最常见的越权来源。
//! 4. **不拒绝未知字段**：`serde` 默认忽略 DTO 里没有的键，所以「DTO 与 DB model
//!    共用」时多传的 `role` / `is_admin` 会被原样接收（批量赋值）。DTO 只放**允许
//!    客户端写**的字段，必要时加 `#[serde(deny_unknown_fields)]`。

mod extractor;
#[cfg(feature = "metrics")]
pub(crate) mod metrics;
pub mod options;
mod rules;
mod validate;

pub use extractor::{Validated, ValidatedForm, ValidatedPath, ValidatedQuery};
pub use options::{DEFAULT_MAX_ERRORS, MAX_ERRORS_HARD_CAP, ValidationOptions};
pub use validate::{validate, validate_with};
