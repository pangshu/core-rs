//! 认证身份 [`Identity`]：跨认证方式（session / jwt / oauth2）的统一产物。
//!
//! 认证只回答「你是谁」——统一产出 `Identity`（用户 id、角色、claims），
//! 由 `middleware/auth` 注入 extension，业务侧只认 `CurrentUser` 提取器。

/// 认证身份（框架统一产物；session 存储需要 serde 序列化）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)] // 派生调试/克隆与 serde 序列化，供 session 缓存存取
pub struct Identity { // 定义跨认证方式统一的身份结构
    /// 用户标识（业务侧的 user id 字符串化）
    pub id: String, // 用户唯一标识
    /// 角色列表（authz 层与业务权限判断共用）
    pub roles: Vec<String>, // 角色名列表
    /// 完整 claims（JWT 自定义字段 / OAuth2 userinfo / session 快照）
    pub claims: serde_json::Value, // 原始 claims JSON，保留方式特有字段
    /// 认证来源："jwt" | "session" | "oauth2:<provider>"
    pub source: String, // 标记本次身份由哪种方式识别
}

impl Identity { // 为 Identity 提供构造与链式扩展
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Self { // 构造最小身份：仅 id 与来源
        Self { // 组装结构体
            id: id.into(), // 转换并存入用户标识
            roles: Vec::new(), // 角色先置空，可由 with_roles 补充
            claims: serde_json::Value::Null, // claims 先置 null，可由 with_claims 补充
            source: source.into(), // 转换并存入认证来源
        }
    }

    pub fn with_roles(mut self, roles: Vec<String>) -> Self { // 链式设置角色列表
        self.roles = roles; // 覆盖角色字段
        self // 返回自身以支持链式调用
    }

    pub fn with_claims(mut self, claims: serde_json::Value) -> Self { // 链式设置完整 claims
        self.claims = claims; // 覆盖 claims 字段
        self // 返回自身以支持链式调用
    }
}
