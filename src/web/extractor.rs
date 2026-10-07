//! 增强提取器：PageQuery（分页参数）、ClientIp（客户端 IP）、CurrentUser（当前用户）。

use axum::extract::{FromRequestParts, Query}; // 引入「从请求部件提取」trait 与查询串提取器
use axum::http::request::Parts; // 引入请求部件类型，提取器签名的入参
use serde::Deserialize; // 引入反序列化派生宏，用于查询参数解析

use crate::error::{AppError, AppResult}; // 引入统一错误类型与结果别名

fn default_page() -> u64 { // serde 缺省值函数：页码默认值
    1 // 默认从第 1 页开始
}
fn default_size() -> u64 { // serde 缺省值函数：每页条数默认值
    10 // 默认每页 10 条
}

/// 分页请求参数。handler 签名直接写 `q: PageQuery` 即可。
#[derive(Debug, Clone, Deserialize)] // 派生调试/克隆/反序列化，供 Query 解析
pub struct PageQuery { // 定义分页查询参数结构体
    #[serde(default = "default_page")] // 缺省时调用 default_page 取 1
    pub page: u64, // 页码（1 起始）
    #[serde(default = "default_size")] // 缺省时调用 default_size 取 10
    pub size: u64, // 每页条数
}

impl PageQuery { // 为分页参数提供便捷取值方法
    /// ORM 分页用的 0 起始页码。page 最小按 1 计（0 视为第一页）。
    pub fn page_index(&self) -> u64 { // 换算为 ORM 需要的 0 起始页码
        self.page.max(1) - 1 // 先夹到 ≥1 再减 1，避免 0 下溢
    }

    /// 每页条数，限幅 1..=100（防止超大 size 拖垮数据库）。
    /// 所有分页路径都应经此取值，防止 `LIMIT 0`。
    pub fn limit(&self) -> u64 { // 取受限幅后的每页条数
        self.size.clamp(1, 100) // 夹在 1..=100 之间
    }
}

impl<S> FromRequestParts<S> for PageQuery // 让 PageQuery 可作为提取器参数
where
    S: Send + Sync, // 要求状态类型线程安全
{
    type Rejection = AppError; // 提取失败统一返回 AppError

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> { // 从查询串提取分页参数
        Query::<PageQuery>::from_request_parts(parts, state) // 复用 axum 的 Query 提取器
            .await // 等待解析完成
            .map(|Query(q)| q) // 拆掉 Query 包装取出内部分页参数
            .map_err(|rej| AppError::bad_request(rej.body_text())) // 解析失败转 400 错误
    }
}

/// 客户端 IP：反代头（X-Forwarded-For / X-Real-IP / Forwarded）优先，回退对端地址。
#[derive(Debug, Clone, Copy)] // 派生调试/克隆/复制（IpAddr 可 Copy）
pub struct ClientIp(pub std::net::IpAddr); // 定义客户端 IP 提取器（透明包裹 IpAddr）

impl<S> FromRequestParts<S> for ClientIp // 让 ClientIp 可作为提取器参数
where
    S: Send + Sync, // 要求状态类型线程安全
{
    type Rejection = AppError; // 提取失败统一返回 AppError

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> { // 从请求解析客户端 IP
        let peer = parts // 先取连接对端地址
            .extensions // 访问请求扩展
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() // 取出 axum 注入的连接信息
            .map(|c| c.0.ip()); // 提取对端 IP
        crate::utils::client_ip::resolve(&parts.headers, peer) // 优先按反代头解析，回退对端 IP
            .map(ClientIp) // 包成 ClientIp 提取器
            .ok_or_else(|| AppError::bad_request("cannot resolve client ip")) // 无法解析则返回 400
    }
}

/// 当前用户：auth 中间件注入的 [`Identity`](crate::auth::Identity)。
/// 缺失 / 未认证统一 401，handler 不用再判空。
///
/// ```no_run
/// # use core_rs::prelude::*;
/// # async fn me(user: CurrentUser) -> ApiResult<String> {
/// Ok(ApiResponse::ok(user.id))
/// # }
/// ```
#[derive(Debug, Clone)] // 派生调试/克隆
pub struct CurrentUser { // 定义当前用户提取器结构体
    pub id: String, // 用户唯一标识
    pub roles: Vec<String>, // 用户角色列表
    /// 完整 claims（JWT 自定义字段 / OAuth2 userinfo / session 快照）
    pub claims: serde_json::Value, // 原始身份声明数据
}

impl CurrentUser { // 为当前用户提供构造与角色判断方法
    pub fn from_identity(identity: &crate::auth::Identity) -> Self { // 由认证身份构造当前用户
        Self { // 构造自身实例
            id: identity.id.clone(), // 复制用户 id
            roles: identity.roles.clone(), // 复制角色列表
            claims: identity.claims.clone(), // 复制身份声明
        }
    }

    /// token / 会话中是否携带指定角色
    pub fn has_role(&self, role: &str) -> bool { // 判断是否拥有某个角色
        self.roles.iter().any(|r| r == role) // 角色列表中存在匹配即返回 true
    }

    /// 校验当前用户具备任一角色，不满足返回 403
    pub fn require_any_role(&self, roles: &[&str]) -> AppResult<()> { // 要求命中任一角色，否则 403
        if roles.is_empty() || self.roles.iter().any(|r| roles.contains(&r.as_str())) { // 未限定角色或命中其一即通过
            return Ok(()); // 校验通过
        }
        Err(AppError::forbidden(format!( // 构造 403 错误并带上实际角色
            "requires one of roles {roles:?}, got {:?}", // 错误消息模板：所需角色与实际角色
            self.roles // 实际拥有的角色列表
        )))
    }
}

impl<S> FromRequestParts<S> for CurrentUser // 让 CurrentUser 可作为提取器参数
where
    S: Send + Sync, // 要求状态类型线程安全
{
    type Rejection = AppError; // 提取失败统一返回 AppError

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> { // 从扩展中取出认证身份
        parts // 访问请求部件
            .extensions // 访问请求扩展
            .get::<crate::auth::Identity>() // 取 auth 中间件注入的身份
            .map(CurrentUser::from_identity) // 转换为当前用户
            .ok_or_else(|| AppError::unauthorized("authentication required")) // 缺失则返回 401
    }
}
