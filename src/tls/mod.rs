//! 服务端 TLS（feature = "tls"）：HTTPS 监听 / 多域名证书 / 热更新。
//!
//! 设计边界（文档 08）：
//! - **框架不持有证书、不定义存储约定、不内置任何证书来源实现**；
//! - 证书由业务实现 [`CertProvider`] 交进来，框架只做机械解析与 SNI 分发；
//! - 证书热更新只替换内存中的 [`store::CertStore`]（`ArcSwap` 原子交换），
//!   rustls `ServerConfig` 只构建一次 → **无需重启、不断连接**。
//!
//! ```rust,ignore
//! App::<AppState>::bootstrap().await?
//!     .cert_provider(Arc::new(MyCertProvider { /* 业务数据源 */ }))
//!     .serve().await?;
//! ```

pub mod reload; // 触发编排（轮询 / 目录监听）+ 到期探针
pub mod serve; // rustls ServerConfig 构建 + HTTP → HTTPS 跳转
pub mod store; // CertStore + DynamicResolver + PEM 解析

use std::sync::Arc; // 引入 Arc，跨线程共享来源与证书仓库

use arc_swap::ArcSwap; // 引入 ArcSwap，用于证书仓库的无锁原子替换

pub use store::CertStore; // 对外导出证书仓库

/// 证书来源契约：框架只定义契约与 DTO，不含任何业务 / 存储假设。
/// 业务自行查库 / 读文件 / 调 KMS / 调 API，返回全部证书条目。
#[async_trait::async_trait] // 让 trait 支持 async 方法
pub trait CertProvider: Send + Sync + 'static { // 证书来源契约
    /// 拉取全部证书条目（每次 reload 调用；实现应保持轻量、可重复调用）
    async fn load(&self) -> Result<Vec<CertEntry>, TlsError>; // 返回全部证书条目
}

/// 框架 DTO：一条域名证书（无业务字段）。
#[derive(Debug, Clone)] // 派生调试与克隆
pub struct CertEntry { // 证书条目
    /// 精确域名（`api.example.com`）或通配符域名（`*.example.com`，只匹配一层子域）
    pub domain: String, // 域名
    /// 证书链 PEM（含中间证书）
    pub cert_pem: Vec<u8>, // 证书链 PEM 字节
    /// 私钥 PEM（业务自行决定是否加密）
    pub key_pem: Vec<u8>, // 私钥 PEM 字节
}

/// TLS 子系统错误
#[derive(Debug, thiserror::Error)] // 派生调试与错误实现
pub enum TlsError { // 定义 TLS 错误枚举
    #[error("io error: {0}")] // IO 错误
    Io(#[from] std::io::Error), // 来自 std::io::Error
    #[error("invalid pem: {0}")] // PEM 解析错误
    Pem(String), // PEM 解析失败信息
    #[error("cert/key missing or empty for domain: {0}")] // 证书或私钥缺失
    Missing(String), // 缺失域名
    #[error("crypto error: {0}")] // 加密 provider 错误
    Crypto(String), // provider 报错信息
    #[error("tls config error: {0}")] // TLS 配置错误
    Config(String), // 配置错误信息
}

/// 可热替换的 TLS 状态（仅 TLS 开启时存在，存于 `CoreState.tls`）。
pub struct TlsState { // TLS 状态
    store: Arc<ArcSwap<CertStore>>, // 证书仓库（热替换点）
    provider: Arc<dyn CertProvider>, // 业务交进来的证书来源
}

impl TlsState { // 为 TLS 状态实现核心方法
    /// 用业务来源构造（初始仓库为空，需随后调用 [`TlsState::reload`] 装载）
    pub fn new(provider: Arc<dyn CertProvider>) -> Self { // 构造 TLS 状态
        Self { // 组装字段
            store: Arc::new(ArcSwap::from_pointee(CertStore::default())), // 空仓库
            provider, // 业务来源
        }
    }

    /// 证书仓库句柄（供 rustls resolver 与探针读取）
    pub fn store(&self) -> Arc<ArcSwap<CertStore>> { // 返回仓库句柄
        self.store.clone() // 克隆 Arc（共享同一份）
    }

    /// 从来源取数 → 解析校验 → 重建仓库 → 原子替换。
    /// **任一失败即整体失败并保留旧仓库**（fail-safe），返回成功装载的证书数。
    pub async fn reload(&self) -> Result<usize, TlsError> { // 重新装载证书
        let entries = self.provider.load().await?; // 向业务来源取数（可能失败）
        let count = entries.len(); // 记录条目数
        let store = store::CertStore::build(&entries)?; // 解析并构建新仓库（失败则返回 Err）
        self.store.store(Arc::new(store)); // 原子替换：下一次握手即生效
        Ok(count) // 返回装载数量
    }
}
