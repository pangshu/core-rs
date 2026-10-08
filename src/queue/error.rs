//! 队列统一错误 [`QueueError`]。

/// 队列操作错误
#[derive(Debug, thiserror::Error)] // 派生 Debug 并让 thiserror 生成 Error 实现
pub enum QueueError { // 定义队列统一错误枚举
    #[error("queue backend not configured: {0}")] // 配置缺失/非法时的错误消息模板
    Config(String), // 配置类错误（携带说明文本）
    #[error("no handler registered for topic `{0}`")] // 发布到未注册 topic 的错误模板
    NoHandler(String), // 目标 topic 未注册错误（携带 topic 名）
    #[error("buffer full for topic `{0}` (publish would block, raise [queue.memory].buffer)")] // 内存队列缓冲已满的错误模板
    Full(String), // 缓冲区已满错误（携带 topic 名）
    #[error("topic `{0}` already registered")] // 重复注册 topic 的错误模板
    AlreadyRegistered(String), // topic 重复注册错误（携带 topic 名）
    #[error("queue already closed")] // 队列已关闭时的错误模板
    Closed, // 队列已关闭错误
    #[error("queue backend error: {0}")] // 后端自身报错时的错误模板
    Backend(String), // 后端实现层错误（携带说明文本）
    #[error("queue serialization error: {0}")] // 载荷序列化失败时的错误模板
    Serde(#[from] serde_json::Error), // 由 serde_json 错误自动转换而来
}

impl QueueError {
    #[allow(dead_code)] // 仅在可选后端 feature 关闭的编译组合中使用
    pub(crate) fn feature_disabled(backend: &str) -> Self { // 构造「后端 feature 未启用」错误
        QueueError::Config(format!( // 以配置错误形式返回
            "queue backend `{backend}` requires its feature to be enabled" // 提示需开启对应 feature
        ))
    }
}
