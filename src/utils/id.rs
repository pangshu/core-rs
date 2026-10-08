//! 短 id 生成（uuid v4）：request_id / trace_id / 消息 id 等。

/// 短 id（uuid v4 简单串）：request_id / trace_id / 消息 id 等
pub fn new_id() -> String { // 生成短随机 id（去连字符的 uuid v4）
    uuid::Uuid::new_v4().simple().to_string() // 生成 v4 uuid 并转为无连字符字符串
}
