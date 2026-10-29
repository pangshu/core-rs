//! aurora 版提取器。
//!
//! 包装 axum 的 `Json` / `Query` / `Path` / `Form`，把 `Rejection` 统一映射为
//! [`AppError`]，保证提取器失败也走统一响应体（见 `docs/09` §6）：
//!
//! | 提取器 | 失败场景 | 错误码 |
//! |---|---|---|
//! | `Json` | 请求体解析失败 | `10005`（400） |
//! | `Json` | 缺少 / 错误的 `Content-Type` | `10004`（415） |
//! | `Json` / `Form` | 请求体超限 | `10002`（413） |
//! | `Query` / `Path` | 参数解析失败 | `10001`（400） |
//! | `Form` | 请求体解析失败 | `10005`（400） |

use axum::extract::rejection::{FormRejection, JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::errors::{AppError, ErrorCode};

/// JSON 请求体提取器（失败映射为 [`AppError`]）。
///
/// `Debug` 已脱敏（只打 `<redacted>`）：请求体里常有密码与令牌，
/// 而 `tracing::debug!(?body)` 是最顺手的调试写法。需要原文请显式取字段。
#[derive(Clone, Copy)]
pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let axum::Json(value) = axum::Json::<T>::from_request(req, state)
            .await
            .map_err(map_json_rejection)?;
        Ok(Self(value))
    }
}

/// 查询串提取器（失败映射为 [`AppError`]）。
///
/// `Debug` 已脱敏：查询串同样可能携带 `?access_token=…`。
#[derive(Clone, Copy)]
pub struct Query<T>(pub T);

impl<S, T> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let axum::extract::Query(value) =
            axum::extract::Query::<T>::from_request_parts(parts, state)
                .await
                .map_err(map_query_rejection)?;
        Ok(Self(value))
    }
}

/// 路径参数提取器（失败映射为 [`AppError`]）。
///
/// `Debug` 已脱敏，规则与其他请求输入包装类型一致。
#[derive(Clone, Copy)]
pub struct Path<T>(pub T);

impl<S, T> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let axum::extract::Path(value) = axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map_err(map_path_rejection)?;
        Ok(Self(value))
    }
}

/// 表单请求体提取器（失败映射为 [`AppError`]）。
///
/// `Debug` 已脱敏（表单里最常见的就是登录密码）。
#[derive(Clone, Copy)]
pub struct Form<T>(pub T);

impl<S, T> FromRequest<S> for Form<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let axum::Form(value) = axum::Form::<T>::from_request(req, state)
            .await
            .map_err(map_form_rejection)?;
        Ok(Self(value))
    }
}

// —— 脱敏 `Debug` ——
// 四个包装类型统一脱敏：请求输入里常有凭证（`password` / `token` / 证件号），
// 而 `#[derive(Debug)]` 会让 `tracing::debug!(?req)` 一行把明文写进日志。
// 不加 `T: Debug` 约束——打印的本来就是占位符，让任意 `T` 都能被打印。
// 需要原文时显式取字段：`tracing::debug!(email = %req.0.email)`。

/// 脱敏 `Debug` 的占位文本。
const REDACTED: &str = "<redacted>";

impl<T> std::fmt::Debug for Json<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Json({REDACTED})")
    }
}

impl<T> std::fmt::Debug for Query<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Query({REDACTED})")
    }
}

impl<T> std::fmt::Debug for Path<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Path({REDACTED})")
    }
}

impl<T> std::fmt::Debug for Form<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Form({REDACTED})")
    }
}

/// 把提取器拒绝转成 [`AppError`] 的统一入口。
///
/// **对外只给固定文案**：axum / serde 的 `body_text()` 会回显目标类型的字段名、
/// 枚举取值与用户输入片段（schema disclosure + 反射），只进 debug 日志。
/// 需要字段级提示时由业务在 handler 里用 `AppError::biz_with_details` 提供。
///
/// 注意与校验失败（`10001`）的分工：后者的 `details` 键就是 DTO 字段路径，
/// **属于公开契约**（前端据此定位表单字段），见 `docs/09` §4。
fn reject(kind: &str, code: ErrorCode, detail: &str) -> AppError {
    tracing::debug!(
        extractor = kind,
        code = code.as_i32(),
        detail = %crate::utils::string::log_safe(detail, 512),
        "request extraction rejected"
    );
    AppError::biz(code, code.default_message())
}

fn map_json_rejection(rejection: JsonRejection) -> AppError {
    let code = match &rejection {
        JsonRejection::MissingJsonContentType(_) => ErrorCode::UNSUPPORTED_MEDIA_TYPE,
        JsonRejection::JsonSyntaxError(_) | JsonRejection::JsonDataError(_) => {
            ErrorCode::MALFORMED_BODY
        }
        JsonRejection::BytesRejection(_) => ErrorCode::PAYLOAD_TOO_LARGE,
        _ => ErrorCode::INVALID_PARAM,
    };
    reject("json", code, &rejection.body_text())
}

fn map_query_rejection(rejection: QueryRejection) -> AppError {
    reject("query", ErrorCode::INVALID_PARAM, &rejection.body_text())
}

fn map_path_rejection(rejection: PathRejection) -> AppError {
    reject("path", ErrorCode::INVALID_PARAM, &rejection.body_text())
}

fn map_form_rejection(rejection: FormRejection) -> AppError {
    let code = match &rejection {
        FormRejection::InvalidFormContentType(_) => ErrorCode::UNSUPPORTED_MEDIA_TYPE,
        FormRejection::FailedToDeserializeForm(_)
        | FormRejection::FailedToDeserializeFormBody(_) => ErrorCode::MALFORMED_BODY,
        FormRejection::BytesRejection(_) => ErrorCode::PAYLOAD_TOO_LARGE,
        _ => ErrorCode::INVALID_PARAM,
    };
    reject("form", code, &rejection.body_text())
}
