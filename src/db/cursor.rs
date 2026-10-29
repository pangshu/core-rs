//! 游标分页。
//!
//! 游标对内部值做 **base64 URL-safe（无填充）** 编码，避免暴露内部字段
//! （见 `docs/10` §6）。查询形态：`WHERE {col} > ? ORDER BY {col} ASC LIMIT n+1`。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ModelTrait, QueryFilter, QueryOrder, QuerySelect,
    Select, Value,
};

use crate::errors::{AppError, AppResult};
use crate::utils::pagination::{CursorPage, clamp_page_size};

/// 不透明游标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(String);

impl Cursor {
    /// 由整数主键构造。
    #[must_use]
    pub fn from_id(id: i64) -> Self {
        Self::encode(id.to_string())
    }

    /// 由原始字符串构造（内部值经 base64 编码）。
    #[must_use]
    pub fn encode(value: impl AsRef<str>) -> Self {
        Self(URL_SAFE_NO_PAD.encode(value.as_ref().as_bytes()))
    }

    /// 解析外部传入的游标字符串（校验可解码）。
    ///
    /// # Errors
    /// 非法游标返回 `10001`。
    pub fn parse(value: &str) -> AppResult<Self> {
        let cursor = Self(value.to_string());
        cursor.decode()?;
        Ok(cursor)
    }

    /// 编码后的字符串（写入 `CursorPage::next_cursor`）。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 解出内部原始值。
    ///
    /// # Errors
    /// 非法游标返回 `10001`。
    pub fn decode(&self) -> AppResult<String> {
        let bytes = URL_SAFE_NO_PAD
            .decode(self.0.as_bytes())
            .map_err(|_| invalid_cursor())?;
        String::from_utf8(bytes).map_err(|_| invalid_cursor())
    }

    /// 解出整数主键。
    ///
    /// # Errors
    /// 非法游标或非整数内容返回 `10001`。
    pub fn decode_id(&self) -> AppResult<i64> {
        self.decode()?.parse().map_err(|_| invalid_cursor())
    }
}

fn invalid_cursor() -> AppError {
    AppError::invalid_param("invalid cursor")
}

/// 游标分页查询：按 `column` 升序，取 `after` 之后的 `size` 条。
///
/// 多取一条用于判断是否还有下一页；`next_cursor` 取本页最后一条的 `column` 值。
///
/// # Errors
/// 查询失败返回映射后的 [`AppError`]（`50002`）。
pub async fn paginate<C, E>(
    conn: &C,
    select: Select<E>,
    column: E::Column,
    after: Option<i64>,
    size: u64,
) -> AppResult<CursorPage<E::Model>>
where
    C: ConnectionTrait,
    E: EntityTrait,
{
    let size = clamp_page_size(size);
    let mut query = select.order_by_asc(column);
    if let Some(after) = after {
        query = query.filter(column.gt(after));
    }

    let mut items = query.limit(size + 1).all(conn).await?;

    let has_more = items.len() as u64 > size;
    if has_more {
        items.pop();
    }

    let next_cursor = if has_more {
        items
            .last()
            .and_then(|model| value_as_i64(&model.get(column)))
            .map(|id| Cursor::from_id(id).as_str().to_string())
    } else {
        None
    };

    Ok(CursorPage::new(items, next_cursor))
}

/// 从 SeaORM [`Value`] 中取出 `i64`（支持各整数列型）。
fn value_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::BigInt(Some(v)) => Some(*v),
        Value::Int(Some(v)) => Some(i64::from(*v)),
        Value::SmallInt(Some(v)) => Some(i64::from(*v)),
        Value::TinyInt(Some(v)) => Some(i64::from(*v)),
        Value::Unsigned(Some(v)) => Some(i64::from(*v)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_roundtrip_cursor() {
        let cursor = Cursor::from_id(1024);
        assert_ne!(cursor.as_str(), "1024", "游标应为不透明编码");
        assert_eq!(cursor.decode().unwrap(), "1024");
        assert_eq!(cursor.decode_id().unwrap(), 1024);
        assert_eq!(Cursor::parse(cursor.as_str()).unwrap(), cursor);
    }

    #[test]
    fn should_reject_invalid_cursor() {
        assert!(Cursor::parse("!!!not-base64!!!").is_err());
        assert!(Cursor::encode("abc").decode_id().is_err());
    }
}
