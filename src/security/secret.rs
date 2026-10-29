//! 敏感值包装（已迁移到 [`crate::utils::secret`]，此处保留导入路径）。
//!
//! 迁移原因：`Secret` 只依赖 `zeroize` 与 `utils::ct`，不该被 `security` feature 门控——
//! `auth`（JWT 密钥）、`cache`（连接串）等模块同样需要它。

pub use crate::utils::secret::Secret;
