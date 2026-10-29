//! 分页模型。
//!
//! - [`Page`]：页码分页，携带 `total`。
//! - [`CursorPage`]：游标分页，携带 `next_cursor`。
//!
//! 默认每页 [`DEFAULT_PAGE_SIZE`] 条，上限 [`MAX_PAGE_SIZE`] 条。

use serde::{Deserialize, Serialize};

/// 默认每页条数。
pub const DEFAULT_PAGE_SIZE: u64 = 20;

/// 每页条数上限。
pub const MAX_PAGE_SIZE: u64 = 100;

/// 页码分页结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    /// 当前页数据。
    pub items: Vec<T>,
    /// 当前页码（从 1 开始）。
    pub page: u64,
    /// 每页条数。
    pub size: u64,
    /// 总条数。
    pub total: u64,
}

impl<T> Page<T> {
    /// 构造一页数据。
    #[must_use]
    pub fn new(items: Vec<T>, page: u64, size: u64, total: u64) -> Self {
        Self {
            items,
            page,
            size,
            total,
        }
    }
}

/// 游标分页结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorPage<T> {
    /// 当前页数据。
    pub items: Vec<T>,
    /// 下一页游标；`None` 表示已到末尾。
    pub next_cursor: Option<String>,
}

impl<T> CursorPage<T> {
    /// 构造一页数据。
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }
}

/// 将请求的每页条数收敛到 `[1, MAX_PAGE_SIZE]`。
#[must_use]
pub fn clamp_page_size(size: u64) -> u64 {
    size.clamp(1, MAX_PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_clamp_page_size() {
        assert_eq!(clamp_page_size(0), 1);
        assert_eq!(clamp_page_size(20), 20);
        assert_eq!(clamp_page_size(1000), MAX_PAGE_SIZE);
    }
}
