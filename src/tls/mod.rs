//! 服务端 TLS（feature `tls`）：证书加载、SNI 多域名、热更新与 OCSP 装订。
//!
//! - [`TlsService`]：装配入口，持有证书仓库并提供 `axum_server` 用的 `ServerConfig`
//! - [`serve_tls`]：基于 `axum-server` 的 HTTPS 服务（SNI 多域名）
//! - [`CertStore`] / [`CertProvider`]：证书仓库与自定义来源
//! - `ocsp`（feature `ocsp-stapling`）：OCSP 装订，握手时附带 CA 签发的吊销状态
//!
//! 文件划分：`cert`（解析）/ `store`（仓库）/ `service`（装配）/ `serve`（监听）/
//! `reload`（热更新）/ `ocsp`（装订）。

pub mod cert;
#[cfg(feature = "ocsp-stapling")]
pub mod ocsp;
mod reload;
mod serve;
mod service;
pub mod store;

pub use serve::serve_tls;
pub use service::TlsService;
pub use store::{
    CertEntry, CertProvider, CertStore, FileCertProvider, LoadOptions, wildcard_match,
};
