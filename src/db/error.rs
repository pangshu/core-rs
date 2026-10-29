//! `sea_orm::DbErr` → [`AppError`](crate::errors::AppError) 的映射。

use crate::errors::{AppError, ErrorCode};
use crate::utils::string::log_safe;

/// 单条日志中错误文本的最大字符数。
const MAX_LOG_CHARS: usize = 512;

/// `sea_orm::DbErr` → [`AppError`] 映射（见 `docs/17` §5）。
///
/// - `RecordNotFound` → `30001`（404）
/// - 其余 → `50002`（500）
///
/// **两条分支都只对外给固定文案**：驱动原文含实体名 / 表名 / SQL 片段与绑定值，
/// 只进日志（并做单行化 + 截断，防日志注入与放大）。
impl From<sea_orm::DbErr> for AppError {
    fn from(err: sea_orm::DbErr) -> Self {
        map_db_error(err)
    }
}

/// 映射实现本体（单独成函数便于测试与复用）。
pub(crate) fn map_db_error(err: sea_orm::DbErr) -> AppError {
    match err {
        sea_orm::DbErr::RecordNotFound(detail) => {
            tracing::debug!(
                detail = %log_safe(&detail, MAX_LOG_CHARS),
                "record not found"
            );
            AppError::biz(ErrorCode::NOT_FOUND, ErrorCode::NOT_FOUND.default_message())
        }
        other => {
            tracing::error!(
                error = %log_safe(&other.to_string(), MAX_LOG_CHARS),
                "database error"
            );
            AppError::biz(ErrorCode::DB_ERROR, ErrorCode::DB_ERROR.default_message())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_not_leak_record_not_found_detail() {
        // 驱动原文可能含实体名 / 表名 / 条件片段，不得进入对外文案
        let err = map_db_error(sea_orm::DbErr::RecordNotFound("users.id = 42".to_string()));
        assert_eq!(err.code(), ErrorCode::NOT_FOUND);
        assert_eq!(err.to_string(), ErrorCode::NOT_FOUND.default_message());
        assert!(!err.to_string().contains("users"));
    }

    #[test]
    fn should_not_leak_db_error_detail() {
        let err = map_db_error(sea_orm::DbErr::Custom(
            "FATAL: password authentication failed for user \"app\"".to_string(),
        ));
        assert_eq!(err.code(), ErrorCode::DB_ERROR);
        assert!(!err.to_string().contains("password"));
    }
}
