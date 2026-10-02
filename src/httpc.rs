//! HTTP 客户端封装（feature = "http-client"）：reqwest + 默认超时，
//! JSON 便捷方法。薄封装，`inner()` 随时拿原始 client。

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

fn http_err(e: reqwest::Error) -> AppError {
    AppError::internal(format!("http request failed: {e}"))
}

impl HttpClient {
    /// 构建带整体超时的客户端
    pub fn new(timeout: Duration) -> AppResult<Self> {
        let inner = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| AppError::internal(format!("http client build failed: {e}")))?;
        Ok(Self { inner })
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        self.inner
            .get(url)
            .send()
            .await
            .map_err(http_err)?
            .error_for_status()
            .map_err(http_err)?
            .json::<T>()
            .await
            .map_err(http_err)
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> AppResult<T> {
        self.inner
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(http_err)?
            .error_for_status()
            .map_err(http_err)?
            .json::<T>()
            .await
            .map_err(http_err)
    }

    /// 原始 client，绕过封装自由使用
    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }
}
