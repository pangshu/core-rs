//! 框架级工具：client_ip 解析、雪花 id、时间。不含任何业务词汇。
//!
//! 判断标准（01/02 文档）：把项目名换掉这段代码仍一字不改 → 进框架。

pub mod client_ip; // 声明客户端真实 IP 解析子模块
pub mod snowflake; // 声明雪花 id 生成器子模块
pub mod time; // 声明时间工具子模块

mod id; // 短 id 生成（uuid v4）

pub use id::new_id; // 对外导出短 id 生成函数
