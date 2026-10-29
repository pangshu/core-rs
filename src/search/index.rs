//! 索引封装（Tantivy）。
//!
//! ⚠️ Tantivy 的 `IndexWriter` 对**同一索引目录**加文件锁，全进程只能有一个。
//! 因此 [`SearchIndex`] 内部用 `Arc` 共享（`clone()` 只是复制句柄），并由
//! [`SearchEngine`](crate::search::SearchEngine) 统一缓存，**不要自己重复 open**。
//!
//! 安全相关的三处硬约束都在这里：
//! - `search()` 先做查询串预检（长度 / 嵌套深度 / 通配）与**字段白名单**校验；
//! - `limit` 一律归一到 `[1, max_limit]`；
//! - `add()` 默认**严格映射**（字段缺失/类型不符直接报错，不静默丢弃）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tantivy::collector::TopDocs;
use tantivy::query::Query as _;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, FieldType, Schema, TantivyDocument};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

use crate::errors::{AppError, AppResult, ErrorCode};
use crate::search::document::{SearchDocument, SearchHit};
use crate::search::fsx;
use crate::search::guard;
use crate::search::options::SearchOptions;
use crate::search::query::Query;
use crate::search::schema::{self, register_cjk};

/// 单个索引（一个 schema 版本）；`clone()` 共享同一个 writer / reader。
#[derive(Clone)]
pub struct SearchIndex {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    version: String,
    dir: PathBuf,
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    default_fields: Vec<Field>,
    options: SearchOptions,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field("name", &self.inner.name)
            .field("version", &self.inner.version)
            .field("dir", &self.inner.dir)
            .finish_non_exhaustive()
    }
}

impl SearchIndex {
    /// 打开已存在的索引目录。
    ///
    /// # Errors
    /// 目录不存在/损坏、writer 创建失败（如已被其他实例占用）时返回 `50005`。
    pub(crate) fn open(
        name: &str,
        version: &str,
        dir: &Path,
        options: &SearchOptions,
    ) -> AppResult<Self> {
        let index = Index::open_in_dir(dir).map_err(|error| {
            tracing::error!(path = %dir.display(), %error, "打开索引失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "打开索引失败（索引目录不可用）",
            )
        })?;
        Self::assemble(name, version, dir, index, options)
    }

    /// 在（空）目录上新建索引。
    ///
    /// # Errors
    /// 目录创建/写入失败时返回 `50005`。
    pub(crate) fn create(
        name: &str,
        version: &str,
        dir: &Path,
        schema: Schema,
        options: &SearchOptions,
    ) -> AppResult<Self> {
        std::fs::create_dir_all(dir).map_err(|error| {
            tracing::error!(path = %dir.display(), %error, "创建索引目录失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "创建索引目录失败（检查 [search].index_dir 权限）",
            )
        })?;
        fsx::harden_dir(dir);

        let index = Index::create_in_dir(dir, schema).map_err(|error| {
            tracing::error!(path = %dir.display(), %error, "创建索引失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "创建索引失败（目录中可能已有索引或残留文件）",
            )
        })?;
        Self::assemble(name, version, dir, index, options)
    }

    fn assemble(
        name: &str,
        version: &str,
        dir: &Path,
        index: Index,
        options: &SearchOptions,
    ) -> AppResult<Self> {
        if options.cjk {
            register_cjk(&index);
        }

        let writer: IndexWriter = index
            .writer_with_num_threads::<TantivyDocument>(1, options.writer_memory)
            .map_err(|error| {
                tracing::error!(path = %dir.display(), %error, "创建索引 writer 失败");
                AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    "创建索引 writer 失败（同一索引目录只允许一个 writer，多副本不能共享 index_dir）",
                )
            })?;

        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|error: tantivy::TantivyError| {
                tracing::error!(path = %dir.display(), %error, "创建索引 reader 失败");
                AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "创建索引 reader 失败")
            })?;

        let default_fields = index
            .schema()
            .fields()
            .filter(|(_, entry)| {
                entry.is_indexed() && matches!(entry.field_type(), FieldType::Str(_))
            })
            .map(|(field, _)| field)
            .collect();

        Ok(Self {
            inner: Arc::new(Inner {
                name: name.to_string(),
                version: version.to_string(),
                dir: dir.to_path_buf(),
                index,
                reader,
                writer: Mutex::new(writer),
                default_fields,
                options: *options,
            }),
        })
    }

    /// 索引名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// schema 版本（如 `v1`）。
    #[must_use]
    pub fn version(&self) -> &str {
        &self.inner.version
    }

    /// 索引目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// 是否只有缓存持有这个索引（没有外部 `clone()` 在用）。
    ///
    /// 缓存淘汰只挑这种条目：淘汰一个正在被使用的索引后再 `open` 会撞上
    /// writer 文件锁（同一目录第二个 writer 直接失败）。
    pub(crate) fn is_uniquely_held(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    /// 加入一个文档（需再 [`commit`](Self::commit) 才对查询可见）。
    ///
    /// `[search].strict_mapping = true`（默认）时，字段不在 schema / 值类型不符
    /// **直接报错**；静默丢弃会让「按字段过滤」的文档缺字段，进而漏进越权结果。
    ///
    /// # Errors
    /// 序列化/映射失败 → `10001`；写入失败 → `50005`。
    pub fn add<T: SearchDocument>(&self, document: &T) -> AppResult<()> {
        let tantivy_doc = schema::to_tantivy(
            &self.inner.index.schema(),
            document,
            self.inner.options.strict_mapping,
        )?;
        self.add_document(tantivy_doc)
    }

    /// 加入一个已构造好的 Tantivy 文档（需要 Date / Bytes / IpAddr 等类型时用这个）。
    ///
    /// 文档里出现不属于本索引 schema 的字段会报错——否则可以用另一个 schema
    /// 构造文档把任意字段塞进索引。
    ///
    /// # Errors
    /// 字段越界 → `10001`；写入失败 → `50005`。
    pub fn add_document(&self, document: TantivyDocument) -> AppResult<()> {
        let num_fields = self.inner.index.schema().num_fields();
        for (field, _) in document.field_values() {
            if field.field_id() as usize >= num_fields {
                return Err(AppError::biz(
                    ErrorCode::INVALID_PARAM,
                    "文档包含不属于当前索引 schema 的字段（是否用了另一个索引的 schema 构造？）",
                ));
            }
        }

        let writer = self.lock_writer()?;
        writer
            .add_document(document)
            .map(|_opstamp| ())
            .map_err(|error| {
                tracing::error!(%error, "写入索引失败");
                AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "写入索引失败")
            })
    }

    /// 提交（使写入对查询可见）。
    ///
    /// # Errors
    /// 提交或重载失败 → `50005`。
    pub fn commit(&self) -> AppResult<()> {
        {
            let mut writer = self.lock_writer()?;
            writer.commit().map_err(|error| {
                tracing::error!(%error, "提交索引失败");
                AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "提交索引失败")
            })?;
        }

        self.inner.reader.reload().map_err(|error| {
            tracing::error!(%error, "重载索引 reader 失败");
            AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "重载索引失败")
        })
    }

    /// 检索（在 `spawn_blocking` 中执行 Tantivy 的同步查询）。
    ///
    /// `limit = 0` 时用 `[search].default_limit`；超过 `[search].max_limit` 时
    /// **截断**（`limit` 通常来自客户端，`TopDocs` 会按它预分配内存）。
    ///
    /// `Query::fields` 是**允许字段集合**：查询串里显式写出的其它字段会被拒绝。
    ///
    /// # Errors
    /// 查询语法/字段越权/查询串超限 → `10001`；索引访问失败 → `50005`。
    pub async fn search<T: SearchDocument>(
        &self,
        query: Query,
        limit: usize,
    ) -> AppResult<Vec<SearchHit<T>>> {
        let limit = self.inner.options.resolve_limit(limit);
        let index = self.inner.index.clone();
        let reader = self.inner.reader.clone();
        let default_fields = self.inner.default_fields.clone();
        let options = self.inner.options;

        tokio::task::spawn_blocking(move || {
            blocking_search::<T>(&index, &reader, &default_fields, &query, limit, &options)
        })
        .await
        .map_err(|error| AppError::biz(ErrorCode::INTERNAL, format!("检索任务执行失败：{error}")))?
    }

    /// 文档总数。
    #[must_use]
    pub fn count(&self) -> u64 {
        self.inner.reader.searcher().num_docs()
    }

    /// 可检索的文本字段名（`Query::parse` 未指定字段时使用）。
    #[must_use]
    pub fn default_field_names(&self) -> Vec<String> {
        let schema = self.inner.index.schema();
        self.inner
            .default_fields
            .iter()
            .map(|field| schema.get_field_name(*field).to_string())
            .collect()
    }

    fn lock_writer(&self) -> AppResult<std::sync::MutexGuard<'_, IndexWriter>> {
        self.inner
            .writer
            .lock()
            .map_err(|_| AppError::biz(ErrorCode::INTERNAL, "索引 writer 被污染"))
    }
}

/// 解析「允许字段集合」与「未限定字段时的默认字段」。
fn resolve_fields(
    schema: &Schema,
    default_fields: &[Field],
    query: &Query,
) -> AppResult<(Vec<Field>, HashSet<Field>)> {
    if query.fields().is_empty() {
        // 未指定字段：默认只查可检索文本字段；显式写出的字段则允许任意**已建索引**字段
        // （与 Tantivy 的语义一致，这里只是把它显式化）。
        let allowed = schema
            .fields()
            .filter(|(_, entry)| entry.is_indexed())
            .map(|(field, _)| field)
            .collect();
        return Ok((default_fields.to_vec(), allowed));
    }

    let mut fields = Vec::with_capacity(query.fields().len());
    let mut allowed = HashSet::new();
    for name in query.fields() {
        let field = schema.get_field(name).map_err(|_| {
            AppError::biz(ErrorCode::INVALID_PARAM, format!("索引中不存在字段 {name}"))
        })?;
        if !schema.get_field_entry(field).is_indexed() {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                format!("字段 {name} 未建索引，无法检索"),
            ));
        }
        if allowed.insert(field) {
            fields.push(field);
        }
    }
    Ok((fields, allowed))
}

fn blocking_search<T: SearchDocument>(
    index: &Index,
    reader: &IndexReader,
    default_fields: &[Field],
    query: &Query,
    limit: usize,
    options: &SearchOptions,
) -> AppResult<Vec<SearchHit<T>>> {
    // 1) 资源放大预检：超长 / 深嵌套 / 通配（必须在 parse 之前，解析器是递归下降）
    guard::check_text(query.text(), options)?;

    let schema = index.schema();
    let (fields, allowed) = resolve_fields(&schema, default_fields, query)?;
    if fields.is_empty() {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "索引中没有可检索的文本字段",
        ));
    }

    // 2) 字段越权预检：`body:xxx` 这类显式限定不受 default_fields 约束
    guard::check_fields(query.text(), &schema, &allowed)?;

    let parser = QueryParser::for_index(index, fields);
    let parsed = parser.parse_query(query.text()).map_err(|error| {
        AppError::biz(ErrorCode::INVALID_PARAM, format!("查询语法错误：{error}"))
    })?;

    // 3) 兜底：解析后的查询树里不允许出现白名单外的字段
    //    （`query_terms` 对 Range / All 等类型是空实现，所以前面那步文本预检不能省）
    let mut denied = false;
    parsed.query_terms(&mut |term, _| {
        if !allowed.contains(&term.field()) {
            denied = true;
        }
    });
    if denied {
        return Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "查询字段不在允许范围内",
        ));
    }

    let searcher = reader.searcher();
    let top = searcher
        .search(&parsed, &TopDocs::with_limit(limit))
        .map_err(|error| {
            tracing::error!(%error, "检索失败");
            AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "检索失败")
        })?;

    let mut hits = Vec::with_capacity(top.len());
    for (score, address) in top {
        let document: TantivyDocument = searcher.doc(address).map_err(|error| {
            tracing::error!(%error, "读取命中文档失败");
            AppError::biz(ErrorCode::SERVICE_UNAVAILABLE, "读取命中文档失败")
        })?;
        let json = schema::to_json(&schema, &document);
        let doc: T = serde_json::from_value(json).map_err(|error| {
            AppError::biz(
                ErrorCode::INTERNAL,
                format!("命中文档反序列化失败（检查 schema 字段是否 STORED）：{error}"),
            )
        })?;
        hits.push(SearchHit { score, doc });
    }
    Ok(hits)
}
