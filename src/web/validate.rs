//! garde 集成（文档 三·9）：`ValidatedJson<T>` 完成「提取 → 校验 → 错误映射」。
//!
//! 校验规则写在 DTO 上（`#[garde(...)]`），DTO 仍在应用侧；校验失败把 garde 的
//! `Report` 统一映射成 [`AppError::Validation`]，错误明细进 `ApiResponse.data`，
//! 与手写校验、`Biz` 业务码共存。框架走无上下文路径（`Context = ()`）。

use axum::extract::{FromRequest, Request}; // 引入「从整个请求提取」trait 与请求类型
use axum::Json; // 引入 axum 的 JSON 提取器，复用其反序列化
use serde::de::DeserializeOwned; // 引入可反序列化约束 trait

use crate::error::AppError; // 引入统一错误类型，用于错误映射

/// `Json<T>` + `garde::Validate` 组合提取器。用法：`ValidatedJson(dto): ValidatedJson<CreateUser>`。
#[derive(Debug, Clone)] // 派生调试/克隆
pub struct ValidatedJson<T>(pub T); // 定义「先解析再校验」的 JSON 提取器（透明包裹 T）

impl<S, T> FromRequest<S> for ValidatedJson<T> // 让 ValidatedJson 可作为提取器参数
where
    S: Send + Sync, // 要求状态类型线程安全
    T: DeserializeOwned + garde::Validate<Context = ()>, // T 需可反序列化且实现无上下文校验
{
    type Rejection = AppError; // 提取失败统一返回 AppError

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> { // 先解析 JSON 再执行校验
        let Json(value) = Json::<T>::from_request(req, state) // 复用 axum 的 Json 提取器解析请求体
            .await // 等待解析完成
            .map_err(|rej| AppError::bad_request(rej.body_text()))?; // 解析失败转 400 并提前返回
        value.validate()?; // 执行 garde 校验，失败经 From 转成 AppError::Validation
        Ok(ValidatedJson(value)) // 校验通过则包装为提取器返回值
    }
}
