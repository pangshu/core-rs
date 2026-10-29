//! 内嵌全文检索（feature `search`，Tantivy + jieba-rs）。
//!
//! - [`SearchEngine`]：索引目录、schema 版本与别名切换（`rebuild` → `activate`）
//! - [`SearchIndex`]：写入（`add` / `commit`）与查询（`search`，内部 `spawn_blocking`）
//! - [`SearchDocument`]：文档抽象（`schema()` 字段名需与 serde 字段一致）
//! - [`Query`]：查询串 + 目标字段（**允许字段集合**）
//! - [`SearchHit`]：命中（得分 + 文档）
//!
//! **中文分词**：`[search].analyzer = "cjk"`（默认）时用 jieba 分词，并**覆盖内置
//! `default`**——用户 schema 里的 `TEXT` 无需感知分词器名即可生效。词典进程内共享
//! 一份（不随打开的索引数增长）。
//!
//! # 安全边界（索引名与查询串通常来自请求，框架在这里兜底）
//!
//! - **索引名是路径片段**：只允许 `[A-Za-z0-9_-]`、≤64 字节；拼接后必须仍在
//!   `[search].index_dir` 内。`../` / 绝对路径 / 分隔符一律 `10001`。
//! - **`Query::fields` 是允许字段集合**，不是「默认字段」：查询串里显式写出的
//!   其它字段会被拒绝（Tantivy 的 `field:term` 不受 default_fields 约束）。
//! - **查询串有长度与嵌套深度上限**，`limit` 有 `max_limit` 上限（防栈溢出与 OOM）。
//! - **写入默认严格映射**：字段拼错 / 类型不符直接报错，不静默丢弃。
//! - **同一 `index_dir` 全进程一个引擎 + 跨进程文件锁**：多副本不能共享索引目录。
//!
//! **不做**：不做分布式检索集群（内嵌引擎，见 `docs/03` §4.14）。

mod fsx;
mod guard;
mod options;

pub mod document;
pub mod engine;
pub mod index;
pub mod query;
pub mod schema;

pub use document::{SearchDocument, SearchHit};
pub use engine::SearchEngine;
pub use index::SearchIndex;
pub use query::Query;
pub use schema::JIEBA_TOKENIZER;
