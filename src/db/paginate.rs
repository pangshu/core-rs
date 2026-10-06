//! 统一分页：「分页参数规范化（page 从 1 起、size 上限裁剪）+ Paginated<T>」，
//! 与 `web::response::Page<T>` 字段对齐（文档 三·5）。

use serde::Deserialize;
use sea_orm::{ConnectionTrait, DbErr, PaginatorTrait};

/// 分页参数（规范化入口）。web 侧的 `PageQuery` 提取器 `From` 转换而来，
/// 也可直接 `Query<PageParams>` 提取。
#[derive(Debug, Clone, Deserialize)]
pub struct PageParams {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_size")]
    pub size: u64,
    /// size 上限（默认 100，防止超大 size 拖垮数据库）
    #[serde(default = "default_max_size", skip_deserializing)]
    pub max_size: u64,
}

fn default_page() -> u64 {
    1
}
fn default_size() -> u64 {
    10
}
fn default_max_size() -> u64 {
    100
}

impl Default for PageParams {
    fn default() -> Self {
        Self {
            page: default_page(),
            size: default_size(),
            max_size: default_max_size(),
        }
    }
}

impl PageParams {
    pub fn new(page: u64, size: u64) -> Self {
        Self { page, size, max_size: default_max_size() }
    }

    /// ORM 分页用的 0 起始页码。page 最小按 1 计（0 视为第一页）。
    pub fn page_index(&self) -> u64 {
        self.page.max(1) - 1
    }

    /// 每页条数：限幅 1..=max_size，防止 `LIMIT 0` / 超大 LIMIT。
    /// 所有分页路径都应经此取值。
    pub fn limit(&self) -> u64 {
        self.size.clamp(1, self.max_size.max(1))
    }
}

impl From<crate::web::extractor::PageQuery> for PageParams {
    fn from(q: crate::web::extractor::PageQuery) -> Self {
        Self::new(q.page, q.size)
    }
}

/// 分页结果（ORM 侧）。`into_page()` 转 `web::response::Page<T>` 直接作为响应 data。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Paginated<T> {
    pub records: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub size: u64,
    pub pages: u64,
}

/// 对任意 select 执行规范化分页（记录数 + 当前页数据）
pub async fn fetch_paginated<E, C>(
    select: sea_orm::Select<E>,
    db: &C,
    q: &PageParams,
) -> Result<Paginated<E::Model>, DbErr>
where
    E: sea_orm::EntityTrait,
    <E as sea_orm::EntityTrait>::Model: Send + Sync,
    C: ConnectionTrait,
{
    let size = q.limit();
    let paginator = select.paginate(db, size);
    let total = paginator.num_items().await?;
    let records = paginator.fetch_page(q.page_index()).await?;
    let page = q.page.max(1);
    Ok(Paginated {
        records,
        total,
        page,
        size,
        pages: if size == 0 { 0 } else { total.div_ceil(size) },
    })
}

impl<T> Paginated<T> {
    /// 转为响应体统一分页结构
    pub fn into_page(self) -> crate::web::response::Page<T> {
        crate::web::response::Page {
            records: self.records,
            total: self.total,
            page: self.page,
            size: self.size,
            pages: self.pages,
        }
    }
}
