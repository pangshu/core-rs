//! 自定义提取器：
//! - [`PageQuery`]：分页参数提取，`?page=1&size=10`，带默认值；
//! - [`ValidJson`]：JSON 反序列化 + validator 校验一步完成，校验失败自动 400。

use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::Json;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use validator::Validate;

use crate::error::AppError;

fn default_page() -> u64 {
    1
}
fn default_size() -> u64 {
    10
}

/// 分页请求参数。handler 签名直接写 `q: PageQuery` 即可。
#[derive(Debug, Clone, Deserialize)]
pub struct PageQuery {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_size")]
    pub size: u64,
}

impl PageQuery {
    /// ORM 分页用的 0 起始页码。page 最小按 1 计（0 视为第一页）。
    pub fn page_index(&self) -> u64 {
        self.page.max(1) - 1
    }

    /// 每页条数，限幅 1..=100（README 约定 + 防止超大 size 拖垮数据库）。
    /// 所有分页路径都应经此取值，防止 `LIMIT 0`。
    pub fn limit(&self) -> u64 {
        self.size.clamp(1, 100)
    }
}

impl<S> FromRequestParts<S> for PageQuery
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<PageQuery>::from_request_parts(parts, state)
            .await
            .map(|Query(q)| q)
            .map_err(|rej| AppError::bad_request(rej.body_text()))
    }
}

/// `Json<T>` + `Validate` 组合提取器。用法：`ValidJson(dto): ValidJson<CreateUser>`。
#[derive(Debug, Clone)]
pub struct ValidJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Validate,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state)
            .await
            .map_err(|rej| AppError::bad_request(rej.body_text()))?;
        value.validate()?;
        Ok(ValidJson(value))
    }
}
