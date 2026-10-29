//! 校验提取器：反序列化 + 校验，失败自动 400。
//!
//! 复用 crate 的 [`Json`](crate::http::Json) / [`Query`](crate::http::Query) /
//! [`Path`](crate::http::Path) / [`Form`](crate::http::Form) 提取器
//! （`Rejection = AppError`），因此「体解析失败」（`10005`）与「校验失败」
//! （`10001`）都产出统一响应体。
//!
//! # 为什么四种输入都要有
//!
//! `docs/11` 的契约是「所有外部输入经 `validation/` 校验」。只提供 `Validated<T>`
//! （body）时，`?per_page=100000`、`/users/-1` 这类 query / path 输入会**绕过**校验
//! 直接进业务——深分页打库、负数 id 走错分支都是现成的入口。四个提取器同名同构，
//! 让「用框架提取器 = 已校验」成为事实而不是纪律。
//!
//! # 日志安全
//!
//! 所有包装类型的 `Debug` **一律脱敏**（只打 `<redacted>`）。请求体里常有密码、
//! 令牌、身份证号，而 `tracing::debug!(?req)` 是最顺手的调试写法——默认展开等于
//! 给出一条「一行代码泄露明文凭证」的捷径。需要原文时显式取字段：
//! `tracing::debug!(email = %req.0.email)`。

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::errors::AppError;
use crate::http::{Form, Json, Path, Query};
use crate::validation::validate;

/// 脱敏 `Debug` 的占位文本。
const REDACTED: &str = "<redacted>";

/// 校验提取器：`Json` 反序列化 + `T::validate()`。
///
/// ```ignore
/// #[derive(Deserialize, Validate)]
/// struct SignupReq {
///     #[validate(email)]
///     email: String,
/// }
///
/// async fn signup(Validated(req): Validated<SignupReq>) -> AppResult<ApiResponse<()>> { ... }
/// ```
#[derive(Clone, Copy)]
pub struct Validated<T>(pub T);

/// 查询串校验提取器：`Query` 反序列化 + `T::validate()`。
///
/// 用于 `?page=1&per_page=20` 这类**必须**设上限的参数——没有它，
/// `per_page` 就是攻击者直连数据库的压力旋钮。
#[derive(Clone, Copy)]
pub struct ValidatedQuery<T>(pub T);

/// 路径参数校验提取器：`Path` 反序列化 + `T::validate()`。
#[derive(Clone, Copy)]
pub struct ValidatedPath<T>(pub T);

/// 表单校验提取器：`Form` 反序列化 + `T::validate()`。
#[derive(Clone, Copy)]
pub struct ValidatedForm<T>(pub T);

impl<S, T> FromRequest<S> for Validated<T>
where
    T: DeserializeOwned + validator::Validate,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state).await?;
        validate(&value)?;
        Ok(Self(value))
    }
}

impl<S, T> FromRequestParts<S> for ValidatedQuery<T>
where
    T: DeserializeOwned + validator::Validate,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(value) = Query::<T>::from_request_parts(parts, state).await?;
        validate(&value)?;
        Ok(Self(value))
    }
}

impl<S, T> FromRequestParts<S> for ValidatedPath<T>
where
    // `http::Path` 的 `FromRequestParts` 额外要求 `T: Send`（路径参数会被跨 await 持有）
    T: DeserializeOwned + validator::Validate + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(value) = Path::<T>::from_request_parts(parts, state).await?;
        validate(&value)?;
        Ok(Self(value))
    }
}

impl<S, T> FromRequest<S> for ValidatedForm<T>
where
    T: DeserializeOwned + validator::Validate,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Form(value) = Form::<T>::from_request(req, state).await?;
        validate(&value)?;
        Ok(Self(value))
    }
}

// —— 脱敏 `Debug` ——
//
// 不加 `T: Debug` 约束：既能给任意 `T` 用，也避免下游因「T 没实现 Debug」
// 而失去对包装类型的调试能力（打印的本来就是占位符）。

impl<T> std::fmt::Debug for Validated<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Validated({REDACTED})")
    }
}

impl<T> std::fmt::Debug for ValidatedQuery<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ValidatedQuery({REDACTED})")
    }
}

impl<T> std::fmt::Debug for ValidatedPath<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ValidatedPath({REDACTED})")
    }
}

impl<T> std::fmt::Debug for ValidatedForm<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ValidatedForm({REDACTED})")
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use validator::Validate;

    use super::*;

    #[derive(Debug, Deserialize, Validate)]
    struct LoginReq {
        #[validate(email)]
        email: String,
        #[validate(length(min = 8))]
        password: String,
    }

    #[test]
    fn should_not_leak_body_through_debug() {
        let req = Validated(LoginReq {
            email: "a@b.com".to_string(),
            password: "hunter2hunter2".to_string(),
        });
        let rendered = format!("{req:?}");
        assert!(!rendered.contains("hunter2"), "Debug 不得输出明文凭证");
        assert!(rendered.contains("redacted"), "应显式标注已脱敏");
    }

    #[test]
    fn should_render_redacted_without_debug_bound() {
        // `T` 未实现 `Debug` 也能打印包装类型
        struct NotDebug(#[allow(dead_code)] u8);
        let req = Validated(NotDebug(1));
        assert_eq!(format!("{req:?}"), "Validated(<redacted>)");
    }
}
