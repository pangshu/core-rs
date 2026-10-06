//! 框架级工具：client_ip 解析、雪花 id、时间。不含任何业务词汇。
//!
//! 判断标准（01/02 文档）：把项目名换掉这段代码仍一字不改 → 进框架。

pub mod client_ip;
pub mod snowflake;
pub mod time;

/// 短 id（uuid v4 简单串）：request_id / trace_id / 消息 id 等
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
