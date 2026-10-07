//! CSRF 中间件（feature = "csrf"，文档 三·16）：双提交 Cookie + 可选签名。
//!
//! - Cookie 里的 `csrf_token` 与请求头 `X-CSRF-Token` 必须一致才放行不安全方法；
//!   GET/HEAD/OPTIONS（可配豁免）与无 Cookie 的纯 API 流量默认跳过（双提交的
//!   前提是 token Cookie 已下发——Cookie 都不存在就不存在可伪造的会话形态）；
//! - **签名模式**（推荐，`[server.csrf].secret` 非空）：token 形如
//!   `{nonce}.{exp}.{hmac}`，HMAC-SHA256 覆盖 `nonce‖exp‖会话上下文`——
//!   伪造/固定（attacker 预置 Cookie）、过期、跨会话盗用的 token 一律 403，
//!   会话上下文取 session Cookie 值（无会话为 "anon"），因此登录/登出后需
//!   重新下发 token。secret 为空时退回纯相等比较（兼容模式，仅记 warn）；
//! - **Origin 防线**（可选，`allowed_origins` 非空）：浏览器带来的 `Origin`
//!   头必须命中列表；非浏览器客户端通常不带 Origin，由签名 token 兜底。
//!
//! token 生成与下发：登录页 / 初始化接口调用 [`new_signed_token`] 生成并经
//! [`issue_cookie`] Set-Cookie（HttpOnly=false 供前端 JS 读取放入请求头）。
//!
//! 依赖锚点：经请求 extension 读取 `CoreState`（`App::serve` 挂在最外层；
//! 裸模式自组装时同样由框架必需件保证存在，缺扩展时 500 fail-closed）。

use axum::extract::{Extension, Request}; // 引入扩展提取器与请求体类型
use axum::http::{header, HeaderValue, StatusCode}; // 引入头常量、头值类型与状态码
use axum::middleware::Next; // 引入 Next，用于把请求交给下游中间件
use axum::response::{IntoResponse, Response}; // 引入响应转换 trait 与响应类型

use crate::state::CoreState; // 引入框架核心状态（依赖锚点）
use crate::traits::HasConfig; // 引入状态能力 trait：取配置

/// 生成随机 nonce（未签名的裸 token，兼容模式用）
pub fn new_token() -> String { // 生成随机裸 token
    uuid::Uuid::new_v4().simple().to_string() // 用 v4 UUID 去连字符作随机串
}

/// 生成签名 token：`{nonce}.{exp}.{hmac}`。
/// `context` 为会话上下文（建议传 session id；无会话传 "anon"）；
/// `ttl_secs` 为有效期（秒），0 = 不过期。同一 token 只在相同 context 下有效，
/// 会话切换后须重新签发。
pub fn new_signed_token(secret: &str, context: &str, ttl_secs: u64) -> String { // 生成签名 token
    let nonce = new_token(); // 生成随机 nonce
    let exp = if ttl_secs == 0 { // ttl 为 0 表示不过期
        0 // exp 置 0 作为不过期标记
    } else { // 有有效期
        crate::utils::time::now_secs() + ttl_secs as i64 // 当前时间 + ttl 作为过期时间戳
    };
    let data = format!("{nonce}.{exp}.{context}"); // 拼接待签名数据
    let signature = crate::security::crypto::hmac_sha256_hex(secret.as_bytes(), data.as_bytes()) // 计算 HMAC-SHA256
        .unwrap_or_default(); // 计算失败时退化为空签名
    format!("{nonce}.{exp}.{signature}") // 组装最终 token
}

/// 校验签名 token（常量时间比较），合法返回其中的 nonce。
/// 过期、签名不符、非签名格式（兼容模式 token）一律返回 None。
pub fn verify_signed_token(secret: &str, context: &str, token: &str) -> Option<String> { // 校验签名 token
    let (head, signature) = token.rsplit_once('.')?; // 从右切出签名段
    let (nonce, exp_str) = head.split_once('.')?; // 从左切出 nonce 与过期段
    let exp = exp_str.parse::<i64>().ok()?; // 解析过期时间戳，非法即失败
    if nonce.is_empty() || nonce.contains('.') { // nonce 为空或含分隔符
        return None; // 非法 nonce，拒绝
    }
    if exp != 0 && crate::utils::time::now_secs() >= exp { // 非不过期且已过期
        return None; // 过期
    }
    // 重算 HMAC 并与 token 内的签名段做常量时间比较（不给时序侧信道留口子）
    let data = format!("{nonce}.{exp}.{context}"); // 重算待签名数据
    if crate::security::crypto::hmac_sha256_verify(secret.as_bytes(), data.as_bytes(), signature) // 常量时间校验签名
        .ok() // 忽略计算错误
        .filter(|v| *v)? // 仅当校验通过才继续
    {
        Some(nonce.to_string()) // 合法：返回 nonce
    } else {
        None // 不合法：拒绝
    }
}

/// 构造下发 token 的 Set-Cookie 值（SameSite=Strict + Secure，防跨站带出）。
/// `secure`：HTTPS 部署传 true（HTTP 开发环境传 false，否则浏览器拒收）。
pub fn issue_cookie(cookie_name: &str, token: &str, secure: bool) -> String { // 构造 Set-Cookie 值
    if secure { // HTTPS 部署
        format!("{cookie_name}={token}; Path=/; SameSite=Strict; Secure") // 带 Secure 属性
    } else { // HTTP 开发环境
        format!("{cookie_name}={token}; Path=/; SameSite=Strict") // 不带 Secure
    }
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> { // 从请求头解析指定 Cookie 值
    for cookie_header in headers.get_all(header::COOKIE) { // 遍历所有 Cookie 头
        let raw = cookie_header.to_str().ok()?; // 头值转字符串，非法即整体失败
        for pair in raw.split(';') { // 按分号拆分多个键值对
            if let Some((k, v)) = pair.trim().split_once('=') { // 拆出键与值
                if k == name { // 命中目标 Cookie 名
                    return Some(v.trim().to_string()); // 返回去空白后的值
                }
            }
        }
    }
    None // 未找到
}

/// 会话绑定上下文：session Cookie 的值；无会话 Cookie 时 "anon"。
/// 直接从请求头读取（不依赖 auth 中间件与 session feature），零耦合。
fn session_context(headers: &axum::http::HeaderMap, session_cookie_name: &str) -> String { // 取会话上下文
    if session_cookie_name.is_empty() { // 未配置 session Cookie 名
        return "anon".to_string(); // 用 "anon" 作上下文
    }
    cookie_value(headers, session_cookie_name).unwrap_or_else(|| "anon".to_string()) // 取 session 值，缺失则 "anon"
}

pub(crate) async fn handle(Extension(core): Extension<CoreState>, req: Request, next: Next) -> Response { // CSRF 中间件入口
    let cfg = core.config().load(); // 加载配置快照
    let csrf = cfg.server.csrf.clone(); // 克隆 CSRF 配置
    let session_cookie_name = cfg.auth.session.cookie_name.clone(); // 克隆 session Cookie 名
    drop(cfg); // 提前释放配置引用，避免长时间占用
    if !csrf.enabled { // CSRF 未启用
        return next.run(req).await; // 直接放行到下游
    }
    let exempt = csrf // 判断当前方法是否豁免
        .exempt_methods // 遍历豁免方法列表
        .iter() // 取得迭代器
        .any(|m| m.eq_ignore_ascii_case(req.method().as_str())); // 大小写不敏感匹配请求方法
    if exempt { // 命中豁免方法
        return next.run(req).await; // 直接放行到下游
    }

    // Origin 防线（可选）：带 Origin 头时必须命中可信列表。
    // 放在 token 校验之前，跨站请求最 cheap 地拒绝
    if !csrf.allowed_origins.is_empty() { // 配置了可信 Origin 列表
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) { // 取请求 Origin 头
            let trusted = csrf // 判断 Origin 是否可信
                .allowed_origins // 遍历可信列表
                .iter() // 取得迭代器
                .any(|o| o.eq_ignore_ascii_case(origin.trim_end_matches('/'))); // 忽略末尾斜杠与大小写比较
            if !trusted { // 不可信
                return forbidden(); // 直接 403
            }
        }
    }

    let cookie_token = cookie_value(req.headers(), &csrf.cookie_name); // 从 Cookie 取 CSRF token
    // 双提交的前提是 token Cookie 已下发：Cookie 完全不存在说明该客户端
    // 不在受 CSRF 保护的会话形态里（纯 API 流量），跳过而非 403 打挂
    let Some(cookie_token) = cookie_token.filter(|c| !c.is_empty()) else { // 无 Cookie token
        return next.run(req).await; // 跳过 CSRF 校验放行
    };
    let header_token = req // 从请求头取 CSRF token
        .headers() // 访问请求头
        .get(&csrf.header_name) // 按配置的头名取
        .and_then(|v| v.to_str().ok()) // 头值转字符串
        .map(|s| s.trim().to_string()); // 去空白并转 String

    let ok = if csrf.secret.is_empty() { // 未配置 secret：兼容模式
        // 兼容模式：纯相等比较（不签名、不绑会话）。建议配置 secret 升级
        tracing::debug!("csrf: secret not configured, falling back to plain double-submit comparison"); // 记调试日志
        matches!(&header_token, Some(h) if *h == cookie_token) // 头与 Cookie 值相等即通过
    } else { // 配置了 secret：签名模式
        // 签名模式：两侧都必须是当前会话上下文下的合法签名 token 且 nonce 一致
        let context = session_context(req.headers(), &session_cookie_name); // 计算会话上下文
        let cookie_nonce = verify_signed_token(&csrf.secret, &context, &cookie_token); // 校验 Cookie token
        let header_nonce = header_token // 校验请求头 token
            .as_deref() // 取 Option<&str>
            .and_then(|t| verify_signed_token(&csrf.secret, &context, t)); // 校验并取 nonce
        matches!((cookie_nonce, header_nonce), (Some(a), Some(b)) if !a.is_empty() && a == b) // 两侧均合法且 nonce 一致
    };
    if ok { // 校验通过
        next.run(req).await // 放行到下游
    } else { // 校验失败
        forbidden() // 返回 403
    }
}

/// 自组装用层（裸模式）：CSRF 双提交校验。生效与否由 `[server.csrf] enabled`
/// 配置决定（挂载权归代码、生效权归配置，支持热更新）。
pub fn layer() -> super::BoxedLayer { // 返回装箱的 CSRF 层
    super::BoxedLayer::new(axum::middleware::from_fn(handle)) // 装箱屏蔽具体层类型
}

fn forbidden() -> Response { // 构造 403 响应
    ( // 组装元组响应
        StatusCode::FORBIDDEN, // 状态码 403
        axum::Json(crate::web::response::ApiResponse::error( // 使用统一 JSON 错误体
            403, // 业务错误码 403
            "csrf token missing or invalid", // 错误信息
        )),
    )
        .into_response() // 转为 Response
}

#[allow(unused)] // 允许未使用（占位保留 HeaderValue 引用，避免导入告警）
fn _keep(h: Option<HeaderValue>) {} // 占位函数，保持 HeaderValue 导入不被判为未用

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn signed_token_roundtrip() {
        let token = new_signed_token(SECRET, "session-1", 3600);
        let nonce = verify_signed_token(SECRET, "session-1", &token).expect("valid token");
        assert!(!nonce.is_empty());
        // 相同 context 可重复校验
        assert_eq!(verify_signed_token(SECRET, "session-1", &token), Some(nonce));
    }

    #[test]
    fn signed_token_rejects_tamper_wrong_context_unsigned_and_expired() {
        let token = new_signed_token(SECRET, "session-1", 3600);

        // 篡改 nonce / exp / 签名 → None
        let parts: Vec<&str> = token.split('.').collect();
        let tampered = format!("ffffffffffffffffffffffffffffffff.{}.{}", parts[1], parts[2]);
        assert_eq!(verify_signed_token(SECRET, "session-1", &tampered), None);
        let tampered = format!("{}.{}.{}", parts[0], parts[1].parse::<i64>().unwrap() + 1, parts[2]);
        assert_eq!(verify_signed_token(SECRET, "session-1", &tampered), None);

        // 换会话上下文（跨会话盗用）→ None
        assert_eq!(verify_signed_token(SECRET, "session-2", &token), None);

        // 兼容模式的裸 token（无签名）在签名模式下 → None
        assert_eq!(verify_signed_token(SECRET, "session-1", &new_token()), None);

        // ttl=0 表示不过期：手工构造 exp=0 的合法 token 仍可校验
        let forever = new_signed_token(SECRET, "anon", 0);
        assert!(verify_signed_token(SECRET, "anon", &forever).is_some());
    }

    #[test]
    fn signed_token_expires() {
        let token = new_signed_token(SECRET, "anon", 1);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(verify_signed_token(SECRET, "anon", &token), None, "过期 token 必须失效");
    }
}
