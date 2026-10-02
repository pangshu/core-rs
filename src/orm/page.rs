//! 统一分页响应结构，与 [`crate::web::extract::PageQuery`] 配对。

use serde::Serialize;

use crate::web::extract::PageQuery;

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "swagger", derive(utoipa::ToSchema))]
pub struct Page<T> {
    pub records: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub size: u64,
    pub pages: u64,
}

impl<T> Page<T> {
    pub fn new(records: Vec<T>, total: u64, q: &PageQuery) -> Self {
        let pages = if q.size == 0 {
            0
        } else {
            total.div_ceil(q.size)
        };
        Self {
            records,
            total,
            page: q.page,
            size: q.size,
            pages,
        }
    }
}
