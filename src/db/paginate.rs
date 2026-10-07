//! 统一分页：「分页参数规范化（page 从 1 起、size 上限裁剪）+ Paginated<T>」，
//! 与 `web::response::Page<T>` 字段对齐（文档 三·5）。

use serde::Deserialize; // 引入反序列化派生，用于从查询参数解析
use sea_orm::{ConnectionTrait, DbErr, PaginatorTrait}; // 引入连接 trait、错误类型与分页器 trait

/// 分页参数（规范化入口）。web 侧的 `PageQuery` 提取器 `From` 转换而来，
/// 也可直接 `Query<PageParams>` 提取。
#[derive(Debug, Clone, Deserialize)] // 派生调试/克隆/反序列化
pub struct PageParams { // 分页参数结构体
    #[serde(default = "default_page")] // 缺省时用 default_page 填充
    pub page: u64, // 页码，从 1 开始
    #[serde(default = "default_size")] // 缺省时用 default_size 填充
    pub size: u64, // 每页条数
    /// size 上限（默认 100，防止超大 size 拖垮数据库）
    #[serde(default = "default_max_size", skip_deserializing)] // 上限由服务端设定，不从请求反序列化
    pub max_size: u64, // 每页条数上限
}

fn default_page() -> u64 { // page 的默认值函数
    1 // 默认第 1 页
}
fn default_size() -> u64 { // size 的默认值函数
    10 // 默认每页 10 条
}
fn default_max_size() -> u64 { // max_size 的默认值函数
    100 // 默认上限 100 条
}

impl Default for PageParams { // 为 PageParams 实现 Default
    fn default() -> Self { // 默认构造
        Self { // 构造结构体
            page: default_page(), // 默认页码
            size: default_size(), // 默认每页条数
            max_size: default_max_size(), // 默认上限
        }
    }
}

impl PageParams { // PageParams 的方法
    pub fn new(page: u64, size: u64) -> Self { // 用页码与条数构造参数
        Self { page, size, max_size: default_max_size() } // 上限取默认值
    }

    /// ORM 分页用的 0 起始页码。page 最小按 1 计（0 视为第一页）。
    pub fn page_index(&self) -> u64 { // 转为 0 起始页码
        self.page.max(1) - 1 // 页码至少按 1 算再减一
    }

    /// 每页条数：限幅 1..=max_size，防止 `LIMIT 0` / 超大 LIMIT。
    /// 所有分页路径都应经此取值。
    pub fn limit(&self) -> u64 { // 计算实际 LIMIT
        self.size.clamp(1, self.max_size.max(1)) // 把条数限制在 1..=max_size 之间
    }
}

impl From<crate::web::extractor::PageQuery> for PageParams { // 从 web 提取器参数转换
    fn from(q: crate::web::extractor::PageQuery) -> Self { // 实现转换
        Self::new(q.page, q.size) // 用提取到的页码与条数构造
    }
}

/// 分页结果（ORM 侧）。`into_page()` 转 `web::response::Page<T>` 直接作为响应 data。
#[derive(Debug, Clone, serde::Serialize)] // 派生调试/克隆/序列化
pub struct Paginated<T> { // 分页结果结构体
    pub records: Vec<T>, // 当前页记录
    pub total: u64, // 总记录数
    pub page: u64, // 当前页码
    pub size: u64, // 每页条数
    pub pages: u64, // 总页数
}

/// 对任意 select 执行规范化分页（记录数 + 当前页数据）
pub async fn fetch_paginated<E, C>( // 通用分页执行函数
    select: sea_orm::Select<E>, // 待分页的查询
    db: &C, // 数据库连接
    q: &PageParams, // 分页参数
) -> Result<Paginated<E::Model>, DbErr> // 返回分页结果或错误
where // 泛型约束开始
    E: sea_orm::EntityTrait, // 约束：E 为实体
    <E as sea_orm::EntityTrait>::Model: Send + Sync, // 约束：模型可跨线程
    C: ConnectionTrait, // 约束：C 为连接
{
    let size = q.limit(); // 规范化后的每页条数
    let paginator = select.paginate(db, size); // 构造分页器
    let total = paginator.num_items().await?; // 查询总记录数
    let records = paginator.fetch_page(q.page_index()).await?; // 取当前页数据
    let page = q.page.max(1); // 当前页码（至少 1）
    Ok(Paginated { // 组装分页结果
        records, // 当前页记录
        total, // 总记录数
        page, // 当前页码
        size, // 每页条数
        pages: if size == 0 { 0 } else { total.div_ceil(size) }, // 总页数（向上取整）
    })
}

impl<T> Paginated<T> { // 分页结果的方法
    /// 转为响应体统一分页结构
    pub fn into_page(self) -> crate::web::response::Page<T> { // 转成 web 响应分页结构
        crate::web::response::Page { // 构造响应结构
            records: self.records, // 记录
            total: self.total, // 总数
            page: self.page, // 页码
            size: self.size, // 每页条数
            pages: self.pages, // 总页数
        }
    }
}
