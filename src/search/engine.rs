//! 检索引擎：索引目录、schema 版本与别名切换。
//!
//! 目录结构（`[search].index_dir` 下）：
//!
//! ```text
//! {index_dir}/.lock           -> 根目录独占锁（跨进程互斥）
//! {index_dir}/{name}/CURRENT  -> "v2"（当前版本）
//! {index_dir}/{name}/v1/      -> Tantivy 索引（旧 schema）
//! {index_dir}/{name}/v2/      -> Tantivy 索引（新 schema）
//! ```
//!
//! schema 变更流程：`rebuild` 建新版本 → 重新灌数据 + `commit` → `activate`
//! 原子切换 `CURRENT`（查询无中断；见 `docs/07` §3.16）。
//!
//! 引擎**缓存已打开的索引**：Tantivy 的 `IndexWriter` 对同一目录加文件锁，
//! 同一进程内不能重复 open（见 [`SearchIndex`]）。缓存是 LRU，只淘汰
//! 「没有外部 `clone()` 在用」的条目。
//!
//! # 安全约束
//!
//! - **索引名与版本号是路径片段**：只允许 `[A-Za-z0-9_-]`、≤64 字节，且拼接后
//!   必须仍落在 `index_dir` 内（`canonicalize` 前缀校验兜底）。索引名经常来自
//!   请求参数，不校验就等于把「任意目录读写」暴露出去。
//! - **同一 `index_dir` 全进程一个引擎**（进程内注册表 + 根目录文件锁）：多副本
//!   部署共享同一目录会在启动期直接失败，而不是等到第一次写入才撞 writer 锁。

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tantivy::schema::Schema;

use crate::config::SearchSection;
use crate::errors::{AppError, AppResult, ErrorCode};
use crate::search::fsx;
use crate::search::index::SearchIndex;
use crate::search::options::SearchOptions;
use crate::utils::string::log_safe;

/// 记录当前版本的文件名。
const CURRENT: &str = "CURRENT";

/// 根目录独占锁的文件名。
const LOCK_FILE: &str = ".lock";

/// 索引名最大字节数。
const MAX_INDEX_NAME_LEN: usize = 64;

/// 版本号上限（留出 `+1` 的余量，避免 `u32` 溢出）。
const MAX_VERSION: u32 = 999_999_999;

/// 检索引擎（内嵌 Tantivy，无需外部服务）。
///
/// `clone()` 共享同一份索引缓存。
#[derive(Clone)]
pub struct SearchEngine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    config: SearchSection,
    options: SearchOptions,
    /// 配置里的根目录（拼路径用，保持配置原样以便对外展示）。
    root: PathBuf,
    /// `root` 的规范化路径（越界校验用）。
    canon_root: PathBuf,
    cache: Mutex<Cache>,
    /// 根目录独占锁；引擎存活期间一直持有。
    _lock: File,
}

/// 已打开索引的 LRU 缓存。
#[derive(Default)]
struct Cache {
    map: HashMap<String, SearchIndex>,
    /// 最旧在前。
    order: VecDeque<String>,
}

impl Cache {
    fn get(&mut self, key: &str) -> Option<SearchIndex> {
        let hit = self.map.get(key).cloned()?;
        self.touch(key);
        Some(hit)
    }

    fn touch(&mut self, key: &str) {
        if let Some(position) = self.order.iter().position(|item| item == key)
            && let Some(item) = self.order.remove(position)
        {
            self.order.push_back(item);
        }
    }

    fn insert(&mut self, key: String, index: SearchIndex, max: usize) {
        if self.map.insert(key.clone(), index).is_some() {
            self.touch(&key);
        } else {
            self.order.push_back(key);
        }
        self.evict(max);
    }

    fn remove(&mut self, key: &str) -> Option<SearchIndex> {
        self.order.retain(|item| item != key);
        self.map.remove(key)
    }

    /// 淘汰到 `max` 条以内。
    ///
    /// 只淘汰「仅缓存持有」的条目：淘汰一个仍被调用方持有的索引后，下一次 `open`
    /// 会撞上同一目录的 writer 文件锁（第二个 writer 直接失败）。全部在用时就
    /// 暂时超限——宁可多占点内存，也不要把请求打成 50005。
    fn evict(&mut self, max: usize) {
        while self.map.len() > max {
            let Some(position) = self
                .order
                .iter()
                .position(|key| self.map.get(key).is_none_or(SearchIndex::is_uniquely_held))
            else {
                break;
            };
            if let Some(key) = self.order.remove(position) {
                self.map.remove(&key);
            }
        }
    }
}

impl std::fmt::Debug for SearchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchEngine")
            .field("root", &self.inner.root)
            .field(
                "opened",
                &self.inner.cache.lock().map(|c| c.map.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl SearchEngine {
    /// 打开引擎（创建根目录）。
    ///
    /// 同一个 `index_dir` 在**同一进程内**只会存在一个引擎实例（重复调用返回
    /// 同一个实例）；**跨进程**会通过根目录文件锁检测出来并失败——多副本部署
    /// 不能共享索引目录。
    ///
    /// # Errors
    /// 根目录无法创建、已被其它进程占用时返回 `50005`。
    pub fn open(config: &SearchSection) -> AppResult<Self> {
        let root = PathBuf::from(&config.index_dir);
        std::fs::create_dir_all(&root).map_err(|error| {
            tracing::error!(path = %root.display(), %error, "创建索引根目录失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "创建索引根目录失败（检查 [search].index_dir 权限）",
            )
        })?;
        fsx::harden_dir(&root);

        let canon_root = std::fs::canonicalize(&root).map_err(|error| {
            tracing::error!(path = %root.display(), %error, "规范化索引根目录失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "索引根目录不可用（无法解析为绝对路径）",
            )
        })?;

        // 查找 + 加锁 + 登记必须是同一个临界区，否则并发 `open` 会互相抢锁。
        let mut registry = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = registry.get(&canon_root).and_then(Weak::upgrade) {
            return Ok(Self { inner: existing });
        }

        let lock = acquire_root_lock(&canon_root)?;
        let inner = Arc::new(EngineInner {
            config: config.clone(),
            options: SearchOptions::from_config(config),
            root,
            canon_root: canon_root.clone(),
            cache: Mutex::new(Cache::default()),
            _lock: lock,
        });
        registry.insert(canon_root, Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    /// 索引根目录。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// 默认别名（`[search].alias`；索引名为空时使用）。
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.inner.config.alias
    }

    /// 索引是否存在（`CURRENT` 已写入且内容合法）。
    #[must_use]
    pub fn exists(&self, name: &str) -> bool {
        self.resolve(name)
            .ok()
            .and_then(|name| self.current(&name))
            .is_some()
    }

    /// 打开已存在的索引（`CURRENT` 指向的版本；已打开的会复用缓存）。
    ///
    /// # Errors
    /// 索引名非法 → `10001`；索引不存在 → `30001`；打开失败 → `50005`。
    pub fn index(&self, name: &str) -> AppResult<SearchIndex> {
        let name = self.resolve(name)?;
        let version = self.current(&name).ok_or_else(|| {
            AppError::biz(
                ErrorCode::NOT_FOUND,
                format!("索引 {name} 不存在，请先 rebuild 并 activate"),
            )
        })?;

        match self.open_cached(&name, &version) {
            Ok(index) => Ok(index),
            Err(error) => {
                // 并发的 `activate` + 版本 GC 可能刚好把目录删掉：重读 CURRENT 再试一次。
                let Some(latest) = self.current(&name) else {
                    return Err(error);
                };
                if latest == version {
                    return Err(error);
                }
                self.open_cached(&name, &latest)
            }
        }
    }

    /// 按 `{name}:{version}` 命中缓存或打开。
    fn open_cached(&self, name: &str, version: &str) -> AppResult<SearchIndex> {
        let key = cache_key(name, version);
        {
            let mut cache = self.cache()?;
            if let Some(hit) = cache.get(&key) {
                return Ok(hit);
            }
        }

        let dir = self.name_dir(name).join(version);
        self.ensure_inside_root(&dir)?;
        let index = SearchIndex::open(name, version, &dir, &self.inner.options)?;

        let max = self.inner.options.cache_max_indexes;
        self.cache()?.insert(key, index.clone(), max);
        Ok(index)
    }

    /// 建一个新版本的索引（**不切换** `CURRENT`）。
    ///
    /// 典型流程：`rebuild` → 重新灌数据 + `commit` → [`activate`](Self::activate)。
    ///
    /// 版本号取「`CURRENT` 与磁盘上已有版本目录的最大值 + 1」，所以上一次重建
    /// 中途失败留下的残目录不会把该索引永久卡死。
    ///
    /// # Errors
    /// 索引名非法 → `10001`；目录/索引创建失败 → `50005`。
    pub async fn rebuild(&self, name: &str, schema: Schema) -> AppResult<SearchIndex> {
        let name = self.resolve(name)?;
        let version = format!("v{}", self.next_version(&name));
        let dir = self.name_dir(&name);
        let version_dir = dir.join(&version);
        let options = self.inner.options;
        let task_name = name.clone();
        let task_version = version.clone();

        let index = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir).map_err(|error| {
                tracing::error!(path = %dir.display(), %error, "创建索引元数据目录失败");
                AppError::biz(
                    ErrorCode::SERVICE_UNAVAILABLE,
                    "创建索引元数据目录失败（检查 [search].index_dir 权限）",
                )
            })?;
            fsx::harden_dir(&dir);
            SearchIndex::create(&task_name, &task_version, &version_dir, schema, &options)
        })
        .await
        .map_err(|error| {
            AppError::biz(ErrorCode::INTERNAL, format!("重建索引任务失败：{error}"))
        })??;

        // 立刻登记：`activate` 后 `index()` 会命中缓存，避免二次 open 撞 writer 锁。
        let max = self.inner.options.cache_max_indexes;
        self.cache()?
            .insert(cache_key(&name, index.version()), index.clone(), max);
        Ok(index)
    }

    /// 原子切换 `CURRENT` 到给定索引版本（查询无中断）。
    ///
    /// 顺序：唯一临时文件 + `fsync` → `rename` → 目录 `fsync` → 清理旧版本目录。
    /// 固定临时文件名会被并发 `activate` 互相截断（切出空的 `CURRENT`，索引直接
    /// 「消失」），所以必须带随机后缀。
    ///
    /// # Errors
    /// 索引不属于当前引擎/版本非法 → `10001`；写入失败 → `50005`。
    pub fn activate(&self, index: &SearchIndex) -> AppResult<()> {
        let name = index.name();
        validate_index_name(name)?;
        let version = index.version();
        if parse_version(version).is_none() {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                "索引版本号非法（应形如 v1）",
            ));
        }

        let name_dir = self.name_dir(name);
        if index.dir() != name_dir.join(version) {
            return Err(AppError::biz(
                ErrorCode::INVALID_PARAM,
                "索引不属于当前引擎，无法激活",
            ));
        }

        std::fs::create_dir_all(&name_dir).map_err(|error| {
            tracing::error!(path = %name_dir.display(), %error, "创建索引元数据目录失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "创建索引元数据目录失败（检查 [search].index_dir 权限）",
            )
        })?;
        fsx::harden_dir(&name_dir);
        self.ensure_inside_root(&name_dir)?;

        // 先写唯一临时文件再 rename：避免读到半截内容，也避免并发互相截断。
        let tmp = name_dir.join(format!(".{CURRENT}.{}.tmp", uuid::Uuid::new_v4()));
        if let Err(error) = fsx::write_new(&tmp, version) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&tmp, name_dir.join(CURRENT)) {
            let _ = std::fs::remove_file(&tmp);
            tracing::error!(path = %name_dir.display(), %error, "切换索引版本失败");
            return Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "切换索引版本失败",
            ));
        }
        fsx::sync_dir(&name_dir);

        self.gc_versions(name, version);
        Ok(())
    }

    /// 索引名为空时回退到 `[search].alias`，并校验合法性。
    fn resolve(&self, name: &str) -> AppResult<String> {
        let name = if name.trim().is_empty() {
            self.inner.config.alias.clone()
        } else {
            name.to_string()
        };
        validate_index_name(&name)?;
        Ok(name)
    }

    /// 索引目录（`index_dir/{name}`）。
    fn name_dir(&self, name: &str) -> PathBuf {
        self.inner.root.join(name)
    }

    /// 读 `CURRENT`。内容非法时按「索引不存在」处理（fail-closed）。
    fn current(&self, name: &str) -> Option<String> {
        let path = self.name_dir(name).join(CURRENT);
        let raw = std::fs::read_to_string(&path).ok()?;
        let version = raw.trim();
        if parse_version(version).is_none() {
            tracing::warn!(
                path = %path.display(),
                content = %log_safe(version, 32),
                "索引版本文件内容非法，按「索引不存在」处理"
            );
            return None;
        }
        Some(version.to_string())
    }

    /// 下一个可用版本号。
    fn next_version(&self, name: &str) -> u32 {
        let mut max = self
            .current(name)
            .and_then(|version| parse_version(&version))
            .unwrap_or(0);
        if let Ok(entries) = std::fs::read_dir(self.name_dir(name)) {
            for entry in entries.flatten() {
                if let Some(number) = entry.file_name().to_str().and_then(parse_version) {
                    max = max.max(number);
                }
            }
        }
        max.saturating_add(1).min(MAX_VERSION)
    }

    /// 清理超出 `[search].keep_versions` 的旧版本目录（保留当前版本）。
    ///
    /// 尽力而为：仍被查询持有的目录在 Windows 上删不掉，只 warn。
    fn gc_versions(&self, name: &str, current: &str) {
        let keep = self.inner.options.keep_versions.max(1);
        let current_number = parse_version(current).unwrap_or(0);

        let Ok(entries) = std::fs::read_dir(self.name_dir(name)) else {
            return;
        };

        let mut stale: Vec<(u32, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let number = parse_version(entry.file_name().to_str()?)?;
                (number != current_number).then(|| (number, entry.path()))
            })
            .collect();
        stale.sort_by_key(|item| std::cmp::Reverse(item.0));

        // 保留「当前版本 + 最新的 keep-1 个旧版本」。
        for (number, path) in stale.into_iter().skip(keep.saturating_sub(1)) {
            let key = cache_key(name, &format!("v{number}"));
            // 仍被调用方持有（有 clone 在用）时不能删：Unix 下会把文件从在用目录里
            // 摘掉，之后的写入直接失败。
            let removable = match self.cache() {
                Ok(mut cache) => {
                    let in_use = cache
                        .map
                        .get(&key)
                        .is_some_and(|index| !index.is_uniquely_held());
                    if !in_use {
                        cache.remove(&key);
                    }
                    !in_use
                }
                Err(_) => true,
            };
            if !removable {
                tracing::debug!(version = number, "旧索引版本仍被使用，跳过清理");
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(&path) {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "清理旧索引版本失败（可能仍被查询占用）"
                );
            }
        }
    }

    /// 纵深防御：路径（若已存在）规范化后必须仍在根目录内。
    ///
    /// 索引名/版本号已经被校验成单段，正常拼接不可能越界；这里防的是「目录被
    /// 换成了指向外部的符号链接」。
    fn ensure_inside_root(&self, path: &Path) -> AppResult<()> {
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return Ok(());
        };
        if canonical.starts_with(&self.inner.canon_root) {
            return Ok(());
        }
        tracing::error!(
            path = %path.display(),
            canonical = %canonical.display(),
            "索引路径越出根目录，已拒绝"
        );
        Err(AppError::biz(
            ErrorCode::INVALID_PARAM,
            "索引路径越出 [search].index_dir",
        ))
    }

    fn cache(&self) -> AppResult<std::sync::MutexGuard<'_, Cache>> {
        self.inner
            .cache
            .lock()
            .map_err(|_| AppError::biz(ErrorCode::INTERNAL, "索引缓存被污染"))
    }
}

/// 进程内引擎注册表（`root` 的规范化路径 → 弱引用）。
fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<EngineInner>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<EngineInner>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 拿根目录的独占锁（跨进程）。
fn acquire_root_lock(root: &Path) -> AppResult<File> {
    let path = root.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| {
            tracing::error!(path = %path.display(), %error, "创建索引目录锁文件失败");
            AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "创建索引目录锁文件失败（检查 [search].index_dir 权限）",
            )
        })?;

    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "索引目录已被占用");
            Err(AppError::biz(
                ErrorCode::SERVICE_UNAVAILABLE,
                "索引目录已被另一个进程占用（多副本部署不能共享 [search].index_dir）",
            ))
        }
    }
}

/// 索引名是否合法（单段路径片段，可直接作为目录名）。
///
/// 允许 `[A-Za-z0-9_-]`，≤64 字节，不以 `-` 开头。`.` / `..` / 分隔符 /
/// 绝对路径全部被这条规则排除。
#[must_use]
pub(crate) fn is_valid_index_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_INDEX_NAME_LEN
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn validate_index_name(name: &str) -> AppResult<()> {
    if is_valid_index_name(name) {
        return Ok(());
    }
    Err(AppError::biz(
        ErrorCode::INVALID_PARAM,
        format!(
            "索引名非法（只允许字母/数字/下划线/连字符，≤{MAX_INDEX_NAME_LEN} 字节，且不以 - 开头）：{}",
            log_safe(name, MAX_INDEX_NAME_LEN)
        ),
    ))
}

/// 解析版本号（`v1` → `1`）；非法返回 `None`。
fn parse_version(version: &str) -> Option<u32> {
    let digits = version.strip_prefix('v')?;
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u32 = digits.parse().ok()?;
    (number > 0).then_some(number)
}

fn cache_key(name: &str, version: &str) -> String {
    format!("{name}:{version}")
}

#[cfg(test)]
mod tests {
    use tantivy::schema::{STORED, Schema, TEXT};

    use super::*;

    fn config(root: &Path) -> SearchSection {
        SearchSection {
            index_dir: root.display().to_string(),
            ..SearchSection::default()
        }
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("aurora-search-{}", uuid::Uuid::new_v4()))
    }

    fn schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_text_field("title", TEXT | STORED);
        builder.build()
    }

    #[tokio::test]
    async fn should_create_version_and_activate() {
        let root = temp_root();
        let engine = SearchEngine::open(&config(&root)).unwrap();

        assert!(!engine.exists("articles"));
        assert!(engine.index("articles").is_err(), "未建索引时应返回 30001");

        let index = engine.rebuild("articles", schema()).await.unwrap();
        assert_eq!(index.version(), "v1");
        assert!(!engine.exists("articles"), "rebuild 不切换 CURRENT");

        engine.activate(&index).unwrap();
        assert!(engine.exists("articles"));

        let opened = engine.index("articles").unwrap();
        assert_eq!(opened.version(), "v1");
        // 再次打开必须命中缓存——否则会因 writer 文件锁（LockBusy）而失败
        let again = engine.index("articles").unwrap();
        assert_eq!(again.version(), "v1");
    }

    #[tokio::test]
    async fn should_increment_version_on_rebuild() {
        let root = temp_root();
        let engine = SearchEngine::open(&config(&root)).unwrap();

        let v1 = engine.rebuild("articles", schema()).await.unwrap();
        engine.activate(&v1).unwrap();

        let v2 = engine.rebuild("articles", schema()).await.unwrap();
        assert_eq!(v2.version(), "v2");
        assert_eq!(
            engine.index("articles").unwrap().version(),
            "v1",
            "未 activate 前仍是旧版本"
        );

        engine.activate(&v2).unwrap();
        assert_eq!(engine.index("articles").unwrap().version(), "v2");
    }

    #[tokio::test]
    async fn should_fall_back_to_alias_for_empty_name() {
        let root = temp_root();
        let engine = SearchEngine::open(&config(&root)).unwrap();
        let index = engine.rebuild("", schema()).await.unwrap();
        engine.activate(&index).unwrap();

        assert!(engine.exists(""), "空索引名应回退到 alias");
        assert_eq!(index.name(), SearchSection::default().alias);
    }

    #[test]
    fn should_reject_path_traversal_index_name() {
        let root = temp_root();
        let engine = SearchEngine::open(&config(&root)).unwrap();

        for name in [
            "../escape",
            "..",
            ".",
            "a/b",
            r"a\b",
            "/tmp/abs",
            r"C:\Windows\Temp",
            "with space",
            "中文",
        ] {
            let error = engine.index(name).unwrap_err();
            assert_eq!(
                error.code(),
                ErrorCode::INVALID_PARAM,
                "索引名 {name:?} 应返回 10001，实际：{error}"
            );
        }

        // 空名字回退到 alias（合法），只是索引还不存在
        assert_eq!(
            engine.index("").unwrap_err().code(),
            ErrorCode::NOT_FOUND,
            "空索引名应回退到 alias 而不是报非法"
        );

        // 校验必须在建目录之前：根目录外不能出现任何东西
        assert!(!root.parent().unwrap().join("escape").exists());
    }

    #[test]
    fn should_reject_tampered_current() {
        let root = temp_root();
        let engine = SearchEngine::open(&config(&root)).unwrap();

        let name_dir = root.join("articles");
        std::fs::create_dir_all(&name_dir).unwrap();
        std::fs::write(name_dir.join(CURRENT), "../../etc").unwrap();

        assert!(!engine.exists("articles"), "非法版本内容应按不存在处理");
        assert_eq!(
            engine.index("articles").unwrap_err().code(),
            ErrorCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn should_reuse_engine_for_same_root() {
        let root = temp_root();
        let first = SearchEngine::open(&config(&root)).unwrap();
        let index = first.rebuild("articles", schema()).await.unwrap();
        first.activate(&index).unwrap();

        // 同进程重复 open 必须复用同一实例，否则第二个 writer 会撞文件锁
        let second = SearchEngine::open(&config(&root)).unwrap();
        assert_eq!(second.index("articles").unwrap().version(), "v1");
    }

    #[tokio::test]
    async fn should_keep_current_version_when_gc_runs() {
        let root = temp_root();
        let mut cfg = config(&root);
        cfg.keep_versions = 1;
        let engine = SearchEngine::open(&cfg).unwrap();

        let v1 = engine.rebuild("articles", schema()).await.unwrap();
        engine.activate(&v1).unwrap();
        drop(v1); // 不再持有旧版本句柄，GC 才能回收

        let v2 = engine.rebuild("articles", schema()).await.unwrap();
        engine.activate(&v2).unwrap();

        assert!(!root.join("articles").join("v1").exists(), "旧版本应被清理");
        assert!(root.join("articles").join("v2").exists());
        assert_eq!(engine.index("articles").unwrap().version(), "v2");
    }

    #[tokio::test]
    async fn should_not_gc_version_still_in_use() {
        let root = temp_root();
        let mut cfg = config(&root);
        cfg.keep_versions = 1;
        let engine = SearchEngine::open(&cfg).unwrap();

        let v1 = engine.rebuild("articles", schema()).await.unwrap();
        engine.activate(&v1).unwrap();

        let v2 = engine.rebuild("articles", schema()).await.unwrap();
        // 仍持有 v1 句柄（可能还在写/查），GC 必须跳过它——删掉会让在用的 writer 失效
        engine.activate(&v2).unwrap();
        assert!(
            root.join("articles").join("v1").exists(),
            "在用版本不能被删"
        );

        drop(v1);
        let v3 = engine.rebuild("articles", schema()).await.unwrap();
        engine.activate(&v3).unwrap();
        assert!(
            !root.join("articles").join("v1").exists(),
            "句柄释放后下一轮 GC 应回收"
        );
    }

    #[test]
    fn should_validate_index_name() {
        assert!(is_valid_index_name("articles"));
        assert!(is_valid_index_name("tenant_42-articles"));
        assert!(!is_valid_index_name(""));
        assert!(!is_valid_index_name(".."));
        assert!(!is_valid_index_name("a/b"));
        assert!(!is_valid_index_name("-leading"));
        assert!(!is_valid_index_name(&"a".repeat(65)));
    }

    #[test]
    fn should_parse_only_canonical_versions() {
        assert_eq!(parse_version("v1"), Some(1));
        assert_eq!(parse_version("v42"), Some(42));
        assert_eq!(parse_version("v0"), None);
        assert_eq!(parse_version("1"), None);
        assert_eq!(parse_version("v"), None);
        assert_eq!(parse_version("v1a"), None);
        assert_eq!(parse_version("../v1"), None);
        assert_eq!(parse_version("v9999999999"), None, "超过 9 位视为非法");
    }
}
